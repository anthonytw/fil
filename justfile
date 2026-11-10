# fil command index. `just` lists recipes.

# Extra flags for `cargo deny` (the advisory database is fetched on each run). `FIL_DENY_FLAGS=--offline
# just ci` checks against the cached database instead: that is NOT a merge pass (CLAUDE.md § Delivery).
deny_flags := env("FIL_DENY_FLAGS", "")

# Where cargo puts its output. The recipes that run a built binary use this, so they work under
# `just vm-run` (docs/dev/vm.md), which gives each worktree its own CARGO_TARGET_DIR.
target_dir := if env("CARGO_TARGET_DIR", "") == "" { source_directory() / "target" } else { env("CARGO_TARGET_DIR") }

default:
    @just --list

# The per-PR checks (CLAUDE.md § Delivery; GitHub's linux-check and macos-check run it). `deny` runs
# first so that a network failure (the advisory database) shows up at once, not after the whole test
# suite; `notices` next, so that a missing cargo-about (the Notices section below) does too. The
# wall-clock tests are not in it: on a shared CI runner they measure the host (docs/dev/timing-tests.md).
ci: deny notices ci-workflow-test exporter-lock fmt-check clippy check-default release-deps release-test test

# Before a release (docs/m4/release.md §9.2), on a quiet machine: `ci` and the wall-clock tests.
ci-release: ci test-wallclock

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

# The vt100 emulator backend must stay out of the release binary (only alacritty ships; DESIGN.md
# §6.5). `cargo tree -i` exits 0 even when it prints "nothing to print" to stderr, so test its
# output: it lists vt100's dependents on stdout when vt100 is in the graph.
release-deps:
    #!/usr/bin/env bash
    set -euo pipefail
    out=$(cargo tree -q -p fil-shell -e normal,build --target all -i vt100)
    test -z "$out" || { echo "release-deps: vt100 is in the release build graph of fil" >&2; exit 1; }
    # The test hooks (FIL_TEST_WIRE_REVISION and friends) must not reach the released binary: a
    # production fil must never honor an environment variable that changes what wire it speaks.
    # fil-forward is checked once fil depends on it (until then `cargo tree -i` finds no such package).
    # fil-client's test-hooks carry the prediction guards' seam and the flush tap (which sees what
    # is written to the terminal: docs/m3/plan.md section 4.3).
    for c in fil-proto fil-daemon fil-transport fil-session fil-forward fil-client; do
      if ! tree=$(cargo tree -q -p fil-shell -e normal,build,features --target all -i "$c" 2>&1); then
        case "$tree" in *"did not match any packages"*) continue ;; esac
        echo "$tree" >&2; exit 1
      fi
      # (The install bootstrap is in every build since the owner approved it, M4 OQ-1, 2026-11-04;
      # crates/fil/tests/remote_commands.rs checks what reaches ssh; release-check's `sh -c` scan is
      # a tripwire.)
      hooks=$(grep -E 'feature "test-hooks"' <<<"$tree" || true)
      test -z "$hooks" || { echo "release-deps: $c has a test-only feature in the release build of fil: $hooks" >&2; exit 1; }
    done

# Every test but the wall-clock ones (named wallclock_*, below), in parallel. --no-fail-fast: one
# failing test binary does not hide the rest (a CI run reports every failure); the recipe still
# fails on any.
test:
    cargo test --workspace --all-features --no-fail-fast -- --skip wallclock_

# The wall-clock tests, one at a time (docs/dev/timing-tests.md): tests named wallclock_*, whose
# pass or fail depends on real elapsed time (a latency percentile, "at once", Ctrl-C within 3 s).
# Run beside the rest of the suite they measure the contention between tests (a CI container
# capped at a few CPUs is throttled by the kernel in 100 ms periods); alone they measure the
# product. `test` and this recipe together run every test once: `ci-release` runs both. Name a new
# wall-clock test with the prefix and it is in this step and out of the other.
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

# Slow link-simulator soak: many seeds, harsh conditions (not part of `ci`).
netsim-soak:
    cargo test -p fil-netsim --test quic --release -- --ignored --nocapture soak

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

# The forward suite at full size (crates/fil-forward/tests/forward.rs): 256 MiB each way through a
# forward, 20,000 sequential connections, on link::mem, the pipe and QUIC. `just ci` runs the same
# tests at 8 MiB and 2,000 (and 300 concurrent off QUIC). Release build. Also runs the SOCKS fuzz
# table and the new-epoch race with more seeds and runs. One test at a time: at these sizes,
# tests side by side overflow loopback listen queues (128 on macOS) and the ephemeral ports. The
# wall-clock tests are skipped (macos-check runs this on shared runners); `just ci-release` runs them.
forward-soak mib="256" seq="20000":
    FIL_FORWARD_TEST_LIMIT=3600 FIL_FORWARD_BULK_MIB={{mib}} FIL_FORWARD_SEQ={{seq}} FIL_FORWARD_CONCURRENT=1000 FIL_FORWARD_EPOCH_RUNS=150 FIL_SOCKS_SEEDS=500000 cargo test --release -p fil-forward -- --nocapture --test-threads=1 --skip wallclock_

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
predict-soak seeds="200":
    FIL_PREDICT_SEEDS={{seeds}} cargo test --release -p fil-predict --test sim
    FIL_STAMP_SEEDS={{seeds}} cargo test --release -p fil-predict --test screen_view every_row

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
# `just realnet latency` is the latency lab (docs/m2/latency.md), not part of the default run;
# nor is `just realnet predict`, M3's prediction cells (docs/m3/plan.md §5.5).
# The sysd part runs a --privileged container (root on the host) and refuses unless this is a
# GitHub-hosted runner or FIL_REALNET_PRIVILEGED_OK=1 says the machine may be given one.
realnet *args:
    tests/realnet/run.sh {{args}}

# Local Linux VM (docs/dev/vm.md): root-only tests without GitHub. The Lima instance `fil` (override
# with FIL_VM) is Ubuntu aarch64 with Docker, netem and no access to this Mac; code reaches it by
# rsync. GitHub's linux-check stays the x86_64 reference.

# Once after creating the VM: conntrack, the pinned toolchain, just, cargo-deny and cargo-about.
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
# this machine's fwdprobe, built here, and the host's, installed through the gate. Never part of `ci`.
dropin *args:
    cargo build -q --release -p fil-shell --bins
    cargo build -q --release -p fil-shell --example dropin
    cargo build -q --release -p fil-daemon --bin fil-daemon-dev
    cargo build -q --release -p fil-testbins --bin fwdprobe
    just no-test-hooks "{{target_dir}}/release/fil" "{{target_dir}}/release/fil-daemon-dev"
    "{{target_dir}}/release/examples/dropin" {{args}}

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

# Fails if a binary contains the test hooks' variable names (FIL_TEST_*): such a build reads the
# environment to change what wire it speaks and where its sockets are, and must never be the fil
# that is installed or released. Building an example or a test with `-p fil-shell` unifies the dev-
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
          echo "no-test-hooks: $b was built with the test hooks (FIL_TEST_*); rebuild it with its own 'cargo build --release -p fil-shell --bins'" >&2
          exit 1
          ;;
        1) ;;
        *) echo "no-test-hooks: could not read $b (grep status $rc)" >&2; exit 1 ;;
      esac
    done

# ---- Releases (docs/m4/release.md; the scripts are in tests/release/) ----

# The release scripts' own tests: the workflow lint (permissions, pins, no secrets or caches) with a
# mutation for each rule, tag verification (the signers come from main, never from the tag),
# release-check on synthetic releases (archives and .deb and .rpm packages), the checksums, the crate
# publishing script on fakes, and the packaging templates. Part of `ci`. With FIL_RELEASE_TEST_DOCKER=1
# it also assembles archives and packages twice in the pinned images and requires identical hashes
# (pulls the images).
release-test:
    tests/release/selftest.sh

# Build the release the way release.yml does: both Linux binaries in the digest-pinned `cross`
# images (needs `cross` 0.2.5 and Docker; on an arm64 host see docs/m4/release.md section 9), the
# universal macOS binary when run on macOS, then the archives, the source archive and SHA256SUMS
# into OUT/files (assembled by the GNU tar and gzip inside the pinned image). Off macOS, name a macOS
# binary with FIL_MACOS_BINARY=path (a dry run's, for example) or the archives are not assembled.
# With the pinned pandoc on PATH (docs/man/build.sh) the archives carry the man pages, completions and
# example configs, as release.yml's do; without it they do not (and `just release-check` then fails on the missing files).
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
    extra=()
    if ! command -v pandoc > /dev/null; then
      echo "release-build: no docs tree (pandoc 3.13 is not installed; docs/man/install-pandoc.sh does it on Linux): the archives have no man pages or completions" >&2
    elif docs/man/build.sh "$out/docs"; then
      extra=(--extra "$out/docs")
    else
      echo "release-build: docs/man/build.sh failed (see above): the archives have no man pages or completions" >&2
    fi
    tests/release/package.sh --bins "$out/bins" --out "$out/files" ${extra[@]+"${extra[@]}"}
    echo "release-build: the release files are in $out/files; run: just release-check --dry-run $out/files" >&2

# Everything a release candidate must satisfy: `just release-check --dry-run DIR` for a build of a
# branch, `just release-check --tag vX.Y.Z --complete DIR` for a release. One top-level entry per
# archive, the section 4.3 layout, byte-identical bundle copies, the .deb and .rpm packages (exactly
# their archive's files), SHA256SUMS exactly the four archives and the four packages,
# no builder path (/Users/, /home/) or FIL_TEST_ string in a binary, the path remap applied,
# `no-test-hooks` and `release-deps`, static Linux binaries, `fil --version` where it can run, the tag,
# the changelog and the wire. See tests/release/check.sh.
[positional-arguments]
release-check *args:
    tests/release/check.sh "$@"

# The owner's check before signing, and anyone's afterwards: `just release-verify 0.1.0`. Tag and
# SHA256SUMS.sig against .github/release-signers on main, the attestations (narrowly), release-check,
# then an independent rebuild of every Linux binary and archive in the Lima VM under amd64 emulation
# (Rosetta), compared hash for hash. `--dry-run --from-run RUN --ref SHA` (SHA: the full 40-hex commit) checks a dry run instead.
# See tests/release/verify.sh and docs/m4/release.md section 9.
[positional-arguments]
release-verify version *args:
    tests/release/verify.sh "$@"

# Everything a release needs, checked before anything is done (docs/m4/release.md section 9.4):
# the tools, disk, a clean HEAD at Gitea's main, the version and changelog, the lockfiles, GitHub's
# main, gh and ssh as the owner, commit signing, the crates-io environment, the tag ruleset, the
# version still free, Docker, the VM (Rosetta), the release and package keys (present, not unlocked)
# and the Developer ID. Read-only; each failure says what it catches and what to do.
# `--from STEP` checks only what STEP and later steps need; `--list` shows the map.
[positional-arguments]
release-preflight version *args:
    tests/release/release-preflight.sh "$@"

# The one-command release (docs/m4/release.md section 9.5), DETECT-ONLY for now: binds the run to
# VERSION and HEAD, takes the lock, works out from the world (git refs, GitHub's runs and releases,
# crates.io, the tap) which of the seventeen steps are done, runs the preflight for the rest, and
# stops at the next step with what to do by hand (exit 2). `--status` is read-only: each step's
# state and the keys. The actions arrive in later PRs. See tests/release/release.sh.
[positional-arguments]
release version *args:
    tests/release/release.sh "$@"

# Reserve the crates.io names of the nine published crates with a 0.0.1 placeholder each. Whoever holds the
# publishing account runs it, after `cargo login`; the placeholders are built outside the repository, paced
# for the new-crate limit, and names already registered are skipped (docs/m4/release.md section 10.1).
# Dry run by default; `--publish` really publishes.
[positional-arguments]
reserve-crate-names *args:
    tests/release/reserve-crate-names.sh "$@"

# ---- Packaging channels (docs/m4/packaging.md) ----

# The nine published crates package and build for crates.io, in dependency order against a local
# overlay of each other (`cargo publish --workspace --dry-run`); the packaged fil-shell must build.
# Nothing is uploaded and no token is needed (needs network for the crates.io index). Real releases
# are published by release.yml's `crates-io` job, by trusted publishing (N8).
crates-check:
    tests/release/publish-crates.sh --dry-run

# Install-test the .deb and .rpm packages in DIR (a release's files, or `release-build`'s) in Debian
# and Fedora containers: install, `fil --version`, the bundle, the package manager's verify, remove.
# `--arch all` also runs the other architecture where Docker can (Rosetta).
[positional-arguments]
packages-install *args:
    tests/release/install-packages.sh "$@"

# The owner signs the release's two .rpm packages with the package key, on their own machine (never
# CI), in the verified files of `just release-verify VERSION --pre-sign`, then rewrites SHA256SUMS
# and writes SHA256SUMS.asc (docs/m4/release.md section 10.4). Needs gpg and rpm (brew install gnupg rpm).
release-sign-packages version *args:
    tests/release/sign-packages.sh "{{version}}" {{args}}

# The owner signs the macOS binary with the Developer ID and notarizes it, on their own Mac (never
# CI; N4): verifies the draft, signs, requires the stripped result to equal CI's, notarizes, and only
# after Apple accepts rebuilds the macOS archive and SHA256SUMS, signs SHA256SUMS with the release
# key and uploads the three files to the DRAFT (docs/m4/release.md section 10.3). Needs Docker, gh
# and the `fil-notary` notarytool profile. Run it after `just release-verify VERSION --pre-sign`.
release-sign-macos version *args:
    tests/release/sign-macos.sh "{{version}}" {{args}}

# The release's keys, once per session, on the owner's Mac: `just release-keys unlock` puts the release
# key into the ssh-agent with a lifetime, presets the package key's passphrase in gpg-agent (proved by
# a test signature; a reaper forgets it after the TTL) and checks the Developer ID, the notarytool
# profile and a test codesign (options `--ttl 8h`, `--need ssh,gpg,apple`; the one-time gpg-agent setup is
# in docs/m4/release.md section 10.4). Unlock once per release session; `status` and `forget` too.
[positional-arguments]
release-keys *args:
    tests/release/release-keys.sh "$@"

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
# `just release-verify VERSION` verified (and the owner signed). Writes OUT/homebrew/fil.rb, for the
# tap anthonytw/homebrew-tap (docs/m4/packaging.md says how it gets there).
brew-formula version sums=("target/release-verify/" + version + "/files/SHA256SUMS") out="target/packaging":
    packaging/render.sh brew "{{version}}" "{{sums}}" "{{out}}/homebrew"

# The AUR package fil-shell-bin for VERSION: OUT/aur/PKGBUILD and .SRCINFO, from the same verified
# SHA256SUMS. Prepared, not published (docs/m4/packaging.md).
aur-pkgbuild version sums=("target/release-verify/" + version + "/files/SHA256SUMS") out="target/packaging":
    packaging/render.sh aur "{{version}}" "{{sums}}" "{{out}}/aur"

# ---- Notices (docs/m4/release.md section 7) ----

# The cargo-about that renders THIRD-PARTY-NOTICES.md. Pinned: another version can render other
# bytes, and `notices` fails on it. The workflows install exactly this version (install-action);
# locally: cargo install cargo-about --locked --version 0.9.3 --features cli
about_version := "0.9.3"

# THIRD-PARTY-NOTICES.md is current: re-rendered from about.hbs and about.toml for the release graph
# (`-p fil-shell`, default features, normal and build edges, the four release targets of about.toml), it
# is byte for byte the committed file. Fails if a crate in that graph has a license outside the
# allowed list, if about.toml's list is not deny.toml's, if cargo-about warns (a clarify checksum
# that no longer matches) or falls back to a template text ("<year> <copyright holders>"), if a
# crate's NOTICE file is not reproduced, or if the notices miss a crate of `cargo tree`'s graph for
# the same targets. Part of `ci`; needs cargo-about (above) and python3.
notices: (_notices "check")

# Rewrite THIRD-PARTY-NOTICES.md after a dependency change (then review the diff and commit it).
notices-update: (_notices "update")

_notices mode:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{source_directory()}}"
    want={{about_version}}
    have=$(cargo about --version 2>/dev/null | awk '{ print $2 }' || true)
    if [[ "$have" != "$want" ]]; then
      echo "notices: needs cargo-about $want (found '${have:-none}'): cargo install cargo-about --locked --version $want --features cli" >&2
      exit 1
    fi
    # The quoted strings of a TOML array `KEY = [ ... ]` in FILE, sorted.
    list() { awk -v key="$2" '$0 ~ "^"key" = \\[" { on = 1; next } on && /^\]/ { exit } on' "$1" | sed 's/#.*//' | grep -o '"[^"]*"' | tr -d '"' | LC_ALL=C sort; }
    # The allowed licenses: about.toml's `accepted` must be deny.toml's `[licenses] allow`, as sets.
    if [[ -z "$(list deny.toml allow)" ]]; then echo "notices: found no allow list in deny.toml" >&2; exit 1; fi
    if ! diff <(list deny.toml allow) <(list about.toml accepted) > /dev/null; then
      echo "notices: about.toml's accepted list differs from deny.toml's allow list (< deny.toml, > about.toml):" >&2
      diff <(list deny.toml allow) <(list about.toml accepted) >&2 || true
      exit 1
    fi
    targets=$(list about.toml targets)
    [[ -n "$targets" ]] || { echo "notices: about.toml lists no targets" >&2; exit 1; }
    scratch=$(mktemp -d)
    trap 'rm -rf "$scratch"' EXIT
    # cargo-about runs offline (--frozen), so that no license text comes from the network and the
    # output depends only on the lock file and the crates' own files; fetch the crates first.
    cargo fetch --locked -q
    # --fail: a crate whose license is outside the list, or cannot be read, fails the render.
    if ! cargo about generate --frozen --fail -m crates/fil/Cargo.toml -c about.toml about.hbs -o "$scratch/raw.md" 2> "$scratch/about.log"; then
      cat "$scratch/about.log" >&2
      echo "notices: cargo-about failed: a crate in the release graph has a license outside the allowed list (deny.toml, about.toml), or one it cannot read" >&2
      exit 1
    fi
    # cargo-about only warns when a clarification's checksum no longer matches, and then falls back.
    if grep -qE 'WARN|ERROR' "$scratch/about.log"; then
      cat "$scratch/about.log" >&2
      echo "notices: cargo-about warned (a [CRATE.clarify] in about.toml that no longer matches its file?); fix it before rendering" >&2
      exit 1
    fi
    # The workspace's published crates are in cargo-about's data (about.toml, [private]); about.hbs
    # leaves out each crate and license entry without a registry source. That is exact only if no
    # license entry, and no license kind, holds both workspace crates and registry crates.
    cargo about generate --frozen --format json -m crates/fil/Cargo.toml -c about.toml -o "$scratch/about.json" 2> "$scratch/about-json.log" \
      || { cat "$scratch/about-json.log" >&2; echo "notices: cargo-about's JSON render failed" >&2; exit 1; }
    python3 -I tests/release/notice_workspace.py "$scratch/about.json"
    # A license file it does not recognize gets the canonical text, with its template fields.
    if grep -nE '<year>|<copyright holders>|<owner>|<name of author>' "$scratch/raw.md" > "$scratch/placeholders"; then
      echo "notices: a license text is a template, not the crate's own notice (add a [CRATE.clarify] to about.toml):" >&2
      cat "$scratch/placeholders" >&2
      exit 1
    fi
    # The ISC crates' per-file copyright lines, and the NOTICE-file check (tests/release/notice_scan.py).
    cargo metadata --locked --offline --format-version 1 --manifest-path crates/fil/Cargo.toml > "$scratch/meta.json"
    tr -d '\r' < "$scratch/raw.md" > "$scratch/raw-lf.md"
    python3 -I tests/release/notice_scan.py "$scratch/raw-lf.md" "$scratch/meta.json" > "$scratch/source-notices.md"
    toolchain=$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)
    [[ -n "$toolchain" ]] || { echo "notices: no channel in rust-toolchain.toml" >&2; exit 1; }
    sed "s/@RUST_TOOLCHAIN@/$toolchain/g" "$scratch/raw-lf.md" \
      | awk -v f="$scratch/source-notices.md" '$0 == "@SOURCE_NOTICES@" { while ((getline l < f) > 0) print l; next } { print }' > "$scratch/notices.md"
    if grep -nE '@[A-Z][A-Z_]+@' "$scratch/notices.md" >&2; then echo "notices: a placeholder was not filled in" >&2; exit 1; fi
    # Nothing of this machine may reach the file: it ships in every archive.
    for p in "$PWD" "${CARGO_HOME:-$HOME/.cargo}" "$HOME"; do
      if grep -qF -- "$p/" "$scratch/notices.md"; then echo "notices: the rendered file contains a local path ($p)" >&2; exit 1; fi
    done
    # Coverage: the crate table is exactly the registry crates of the release graph `cargo tree` sees.
    sed -n '/^| Crate | Version | License |$/,/^$/p' "$scratch/notices.md" | tail -n +3 | awk -F' [|] ' 'NF >= 3 { sub(/^[|] /, "", $1); print $1 " v" $2 }' | LC_ALL=C sort -u > "$scratch/listed"
    tflags=$(for t in $targets; do printf -- '--target %s ' "$t"; done)
    # shellcheck disable=SC2086
    cargo tree -q --locked -p fil-shell -e normal,build $tflags --prefix none --format '{p}' | grep -v ' (/' | sed -E 's/( \((\*|proc-macro)\))+$//' | sed '/^$/d' | LC_ALL=C sort -u > "$scratch/graph"
    if [[ ! -s "$scratch/graph" ]] || ! diff "$scratch/graph" "$scratch/listed" > "$scratch/cover.diff"; then
      echo "notices: the notices' crates are not the release graph's (< cargo tree, > notices):" >&2
      cat "$scratch/cover.diff" >&2
      exit 1
    fi
    if [[ "{{mode}}" == update ]]; then
      cp "$scratch/notices.md" THIRD-PARTY-NOTICES.md
      echo "notices: wrote THIRD-PARTY-NOTICES.md ($(wc -l < "$scratch/listed" | tr -d ' ') crates); review the diff and commit it" >&2
    elif cmp -s "$scratch/notices.md" THIRD-PARTY-NOTICES.md; then
      echo "notices: THIRD-PARTY-NOTICES.md is current ($(wc -l < "$scratch/listed" | tr -d ' ') crates)" >&2
    else
      echo "notices: THIRD-PARTY-NOTICES.md is stale or edited by hand; run 'just notices-update' and commit it. The difference:" >&2
      diff -u -L THIRD-PARTY-NOTICES.md -L rendered THIRD-PARTY-NOTICES.md "$scratch/notices.md" | head -n 40 >&2 || true
      exit 1
    fi

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
