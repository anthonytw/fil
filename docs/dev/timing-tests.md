# Wall-clock tests, and the test tiers

Some tests pass or fail on real elapsed time: a latency percentile, a refusal "at once", Ctrl-C
within 3 s. This page says how they are told apart from the rest, where they run, and what a
failure of one means. The reason it exists: the first three runs of `just ci` on the Gitea runner
each failed a different one of them, and fixing them one 30-minute run at a time does not
converge.

## What goes wrong on a shared runner

The CI container sees every core of its host but is capped at 6 CPUs. The kernel enforces the cap
by throttling: when the container's threads have used their quota for a 100 ms period, every one of
them stops until the next. A test binary that runs four tests at once, each on a four-thread
runtime, with the `cargo` and `rustc` of its neighbors on the same runner, reaches the quota
easily. A test that times a 1 ms operation then sees 80 ms. It is not the product: the same
operation takes 0.5 ms (p99) on a quiet host, 100 times under the bound.

Two kinds of bound need telling apart, and they get different treatment.

- **(P) A product claim** that any supported host should meet, stated in DESIGN.md: K1's
  Ctrl-C within 3 s at 1 Mbit/s (§8.5, "Pacing the terminal"). A miss is a bug to investigate, as
  #11 was (the 3.63 s run had no host stall in it). These bounds are never relaxed, never retried,
  and never filtered.
- **(T) A figure tuned on a fast dev machine** with no claim behind it, or a figure chosen to
  separate a bug (a wait for the peer's 100 ms ack timer) from noise (75 ms). A miss on a host
  that stalled during the measurement says nothing about the product.

## The structure

1. **Wall-clock tests are named `wallclock_*`.** `just test` runs every other test, in parallel
   (cargo-nextest, whose profiles in `.config/nextest.toml` leave the prefix out); `just
   test-wallclock` runs the `wallclock_` ones, one at a time (`--test-threads=1`), so they do not
   measure the contention between tests. `just ci` runs only `test` (it is the per-PR check, on
   shared CI runners whose stalls a wall-clock test would measure); `just ci-release` runs both,
   on a quiet machine before a release, so nothing is left out. Name a new wall-clock test with
   the prefix and it is in the serial step and out of the parallel one. Two of
   `fil_testkit::timing`'s own tests carry the prefix too: they assert that a quiet stretch
   records no stall, which a busy host cannot promise. The tiers below say where everything else
   runs.
2. **(T) bounds judge only what the host left alone.** `fil_testkit::timing::Stalls` watches the
   host with an OS thread of its own (never a task of the test's runtime, which the product shares, so a product that blocks a worker is not excused as the host) that ticks every 2 ms and records every
   tick that came more than 8 ms late. A sample whose interval overlaps a stall is measured again
   (`Stalls::disturbed`); the bound is unchanged, and a sample the host did not disturb that
   misses it is a miss, every time. A latency that is slow on a quiet host still fails. A test
   that cannot get enough undisturbed samples fails by saying the host was too busy to judge, which
   names the host and not the product.
3. **A new bound** is classified in its comment: P (cite DESIGN.md) or T (say what it separates the
   bug from). A T bound whose test takes a sample uses `Stalls`.

## Inventory

Tests with a real-time bound under about a second (the ones that can miss on a slow or stalled
host). Margins are from the Apple-silicon dev Mac at `RUST_TEST_THREADS=4` (quiet) and with six busy
loops added (burn), the reps listed in the PR. The aarch64 CI runner is roughly 2 to 3 times slower per
core than that Mac and cannot be reproduced here: a bound with a margin under 3x on the Mac is at
risk there even without a stall, and one with a margin of 100x is at risk only from a stall.

Class P is a DESIGN.md claim; T a tuned figure. "Stalls" marks the tests that discard
disturbed samples. All of these are named `wallclock_*`.

| Test (file) | Bound | Class | Mac margin |
|---|---|---|---|
| `terminal_pacing` `ctrl_c_during_a_flood_*` x4 | Ctrl-C shows within 3 s at 1 Mbit/s | P: K1, DESIGN §8.5 "Pacing the terminal" | 0.7 to 2.3 s quiet, up to 2.5 s with load (1.2x); not filtered |
| `terminal_pacing` `*_never_jumps` x4 | no catch-up on a healthy link (count 0) | T (decisions use real time) | 0 jumps in every run |
| `pace` `a_server_that_speaks_first_is_heard_at_once` x3 | p99 <= 75 ms, <= 1 of 200 over 75 ms | T (separates a wait for the 100 ms ack timer, §8.5 "Acks", from noise) | p99 0.4 to 1.1 ms quiet, up to 12 ms at 16 busy loops (6x); Stalls |
| `pace` `a_bulk_forward_keeps_the_terminal_fast_on_a_slow_pipe` | paced probe median <= 150 ms | T over a §8.5 "Pacing" result (probe ~75 ms, p99 < 100 ms) | 74 to 76 ms (2x) |
| `pace` `parallel_downloads_keep_the_link_full` | probe p50 <= 150 ms, rate >= 90% | T | 72 to 77 ms (2x), rate 99.9%; retries on its own stall watch |
| `pace` `stuck_forwards_hold_neither_*` x3, `stuck_forwards_on_a_slow_pipe` | agent p50 <= 100 ms, echo <= 2 s; paced <= 1.25x unpaced + 300 / 50 ms | T | agent 0.3 ms (300x), echo 30 to 64 ms; slow pipe paced vs unpaced within 0.4 ms |
| `pace` `a_stuck_forward_writing_small_chunks_*` x6 | agent worst <= 60 ms, echo <= 2 s | T | worst 0.3 to 2.7 ms (22x); Stalls |
| `forward` (fil-forward) `stuck_consumer` x3 | terminal probe < 500 ms | T | not printed; passes |
| `forward` (fil-forward) `a_refused_local_connection_is_reset_at_once`, `a_new_epoch_rechecks_*` | < 500 ms, < 1 s | T | passes |
| `forward` (fil-session) `a_detached_remote_refuses_at_once` | median of 5 < 50 ms | T (DESIGN §7: "refuse at once", no figure) | passes; Stalls |
| `forward` (fil-session) `forward_floods_do_not_slow_the_terminal` | echo < 1 s | T | passes |
| `agent` (fil-session) `refused_at_once_while_detached` | < 50 ms | T | passes; Stalls |
| `catch_up_order` x2 | takeover < 200 ms; first output < 1.5 s | T | passes |
| `netcut` (drop-in example) `*_held_for_the_delay_each_way` x2 | each way: in one of 3 runs, median <= 30 + 10 ms and IQR <= 3 ms (never < 30 ms, also checked per PR by the untimed `*_never_early` pair) | T (separates a relay that rounds its hold to a 10 ms tick, IQR about 5 ms, from three thread wake-ups) | IQR 0.02 to 0.1 ms locally; 3.4 to 6.8 ms on Intel macOS beside the rest of the suite |
| `repaint` x2 | repaint within 1.4 s / 1.5 s of the request | T | passes |
| `log_blocking` `a_hung_log_file_does_not_stop_the_daemon` | mean call <= 200 us, p99 < 50 ms, max < 1 s | T (the log never blocks the daemon) | passes; Stalls |
| `ports::probe` x3 | no probe < 50 ms; < per_port budget + 100 ms | T (the budget itself is P) | passes; Stalls on the first |
| `proc` (fil-session) `a_kill_without_a_table_ends_when_its_victims_do` | kill returns < 2 s (grace 5 s) | T (separates waiting out the grace and `KILL_CONFIRM`, 7 s, from a `sleep` dying on a hangup) | milliseconds (about 100x); one sample, no Stalls: only a stall of seconds could miss it |
| `keyprobe` x2 | max echo < 1 s; screen vs bytes p50 within 1 ms | T | passes |

Everything else with a duration in it is either on tokio's paused clock (`start_paused`:
fil-transport's pipe and QUIC link, the dialer, swap, detach and attach tests; deterministic) or a
guard against a hang with a bound of a second or more (`elapsed() < 5 s`, `flush(5 s)`), which only
a hang misses. Those are left as they are.

The Mac did not miss any bound in 3 quiet runs, 3 runs with six busy loops and 2 runs with sixteen
(on 14 cores), unchanged or changed alike. It cannot reproduce the runner's failures, which come
from the container's CPU cap. Since 2026-11-09 the wall-clock tests are not part of the per-PR
checks on shared runners at all; `just ci-release` runs them before a release on the Mac, and the
Linux VM runs `just vm-run just test-wallclock`.

## Tiers

Since the test-suite sweep (2026-11-11) every test and harness runs in one of two tiers: the
per-PR gate (on #81's head: linux-check `check` 9.2 min, `release-test` 10.9 min; macos-check
`check` about 20 min) and the release suite, which runs everything else before a release (an
estimate of about an hour, not yet measured whole). Two names and one knob decide where a test
runs:

- **`wallclock_*`**: alone, in the release suite only (above).
- **`soak_*`**: a test with no small form (the netsim soak): the release suite only.
- **`FIL_TEST_SCALE=smoke|full`** (default `smoke`): every test that has a size picks it with
  `fil_testkit::scale::pick(VAR, smoke, full)`, and a set `VAR` (`FIL_PREDICT_SEEDS`,
  `FIL_FORWARD_SEQ`, `MUX_SEEDS`, ...) overrides both. The drop-in suite takes `--scale` (default
  from the same variable).
- **Logic or OS** (`.config/nextest.toml`): a test of pure logic behaves the same on every
  64-bit little-endian target, so it runs on Linux only; a test that touches the OS (PTYs,
  signals, sockets, processes, files and their modes, rlimits, login shells) runs on Linux and
  Apple silicon. A new test binary is logic unless it is in fil-daemon, fil-session or
  fil-testbins or listed in the `os` profile: list an OS binary there when you add it.
  - OS: every binary of fil-daemon, fil-session and fil-testbins; the library unit tests of
    fil-client, fil-transport, fil-forward and fil-shell; fil-transport `quic_udp`, `pipe`,
    `channel_churn`; fil-forward `forward`, `limits`, `nobufs`, `pace`, `wakeups`, `agent`,
    `agent_constraints`; fil-client `offline`, `tty_pty`, `agent`, `forward`,
    `forward_hostile`, `predict_e2e`, `predict_stack`, `predict_suites`; fil-shell `binary`,
    `install`, `install_finish`, `install_gc`, `install_shells`, `remote_commands`,
    `resize_e2e`, `revision`, `session_cmds`, `trampoline`, `tty_group`, `predict_flag`, and
    the drop-in and Pi harnesses' own tests (`example/dropin`, `example/pi_e2e`).
  - Logic: fil-predict, fil-term, fil-proto, fil-netsim and fil-testkit whole; fil-transport
    `alloc`, `quic_auth`, `quic_link`; fil-forward `socks`; the other fil-client binaries
    (`acks`, `attach`, `control_path`, `detach_delivery`, `predict`, `prompts`, `reconnect`,
    `refusals`, `resize_order`, `status`, `swap`, `terminal_restore`, `forward_flood`); fil-shell
    `dialer`, `engine`, `forward_spec`, `ports_view`, `examples`, `docs_sync` and the `gen_docs`
    example; the doctests. Their file access is reading committed fixtures.

Sizes (smoke / full):

| Test | Variable | Smoke | Full |
|---|---|---|---|
| fil-predict `sim` | `FIL_PREDICT_SEEDS` | 2 | 200 |
| fil-predict `screen_view` `every_row` | `FIL_STAMP_SEEDS` | 12 | 200 |
| fil-predict `status` `random` | `FIL_STATUS_SEEDS` | 6 | 600 |
| fil-predict `keyless_reads` | `FIL_KEYLESS_SEEDS` | 150 | 1,500 |
| fil-proto `mux_model` | `MUX_SEEDS` | 24 | 5,000 |
| fil-transport `channel_churn` (QUIC) | `FIL_CHURN_CHANNELS` | 2,000 | 20,000 |
| fil-forward `forward` bulk (at 2 MiB, twice the default window, `drops_mid_transfer`'s third cut leaves less than a window unsent; the first two still cut with more than a window outstanding) | `FIL_FORWARD_BULK_MIB` | 2 MiB | 256 MiB |
| fil-forward `forward` sequential | `FIL_FORWARD_SEQ` | 1,500 | 20,000 |
| fil-forward `forward` concurrent | `FIL_FORWARD_CONCURRENT` | 300 | 1,000 |
| fil-forward `forward` new-epoch race | `FIL_FORWARD_EPOCH_RUNS` | 10 | 150 |
| fil-forward `forward` test limit (s) | `FIL_FORWARD_TEST_LIMIT` | 120 | 3,600 |
| fil-forward `socks` fuzz | `FIL_SOCKS_SEEDS` | 50,000 | 500,000 |
| drop-in suite | `--scale` | no stress, idle or prediction scenarios but `predict_secret`; 12 s sleeps | everything, 45 s sleeps too |

The sequential forward smoke stays above 1,024 (QUIC's stream credit, which a missing
`Link::finish` runs out of); 300 concurrent connections already overflow macOS's listen queue of
128, the failure that size exists to catch.

**Tier 1, the PR gate** (CLAUDE.md § Delivery; `suite=check`):

| Runner | Job | Contents |
|---|---|---|
| Linux | `linux-check` `check` | `just ci` (deny, notices, the workflow test, the exporter lock, fmt, clippy, the default-features check, release-deps, then every test but `wallclock_` and `soak_` at smoke size, logic and OS, and the doctests); the `ci`-profile no-test-hooks check; fidelity `--no-perf`; the prediction guard sweep; the PTY prediction harness at 2 seeds; the client's PTY tests |
| Linux | `linux-check` `release-test` | the release scripts' own tests, in parallel with `check` |
| macOS arm64 | `macos-check` `check` | clippy; `just test-os` (the OS tests at smoke size); `cargo check --target x86_64-apple-darwin -p fil-shell`; the client's PTY tests; the PTY prediction harness at 2 seeds; `just dropin --local --scale smoke` |
| macOS arm64 | `macos-check` `release-test` | the release scripts' tests, only when the change touches the release paths (what CODEOWNERS owns as deciding a release, the justfile, and the other files selftest.sh reads; `git diff --no-renames` against the merge base with main): the macOS signing fakes are the macOS-only part |
| macOS Intel | none | (suite=all) |

The harness builds use the `ci` cargo profile (the release profile without fat LTO, 16 codegen
units): they test behavior and feature unification, not the shipped bytes. In tests, the
emulators and the prediction engine are built at `opt-level = 2` (root `Cargo.toml`), which
took fil-predict's simulator from minutes to seconds.

**Tier 2, the release suite.** `just ci-release`, on a quiet Mac (estimated at about an hour; the
first full-scale pass of `test-release` took 17 min on the Mac, most of it the prior-ack rule at
200 seeds; that test now runs 16 seeds at full size, about 86 s on the Mac, and the whole pass has
not been re-measured since; `forward-soak` took 83 s). Every part runs even when an earlier one fails, and the
recipe ends by listing the parts that failed: `just ci`;
`release-test`; `test-release` (every test at `FIL_TEST_SCALE=full` in the `ci` profile, the
`soak_` tests included, then fil-forward's full sizes one test at a time, `forward-soak`);
`test-wallclock`; the client's PTY tests; the guard sweep; the shipped (fat-LTO) build's
no-test-hooks check (`shipped-no-test-hooks`); full fidelity; the PTY prediction harness at its
defaults; the prediction matrix (`predict-suites all`); terminal pacing; and the full drop-in
suite (`dropin --local --scale full`). On GitHub: `linux-check` `suite=all` (check, release-test,
`full-scale`, which is `test-release` on Linux, realnet, aarch64, `arm64-remote`, terminals; `arm64-remote` is the static aarch64 build as the remote host on GitHub's arm64 runner, driven by `pi_e2e` and the full drop-in suite with prediction on, which replaced the test Pis as a release gate on 2026-11-11, `docs/m1/pi.md`) and `macos-check`
`suite=all` on Apple silicon AND Intel (every test, the full drop-in suite, release-test, the
shipped no-test-hooks check; remote; brew with a release run). The `full-scale` job's 60 min and
the suite=all limits are estimates until a run measures them. release.yml runs release-test
again for the release itself.

Where each step that left the per-PR gate runs now:

| Test or step | Before (per PR) | Now |
|---|---|---|
| `release-test` | inside `just ci` on Linux, arm64 and Intel | linux-check's parallel `release-test` job; macos-check's `release-test` job on arm64 only on release paths; suite=all on both Macs; `ci-release`; release.yml |
| pure-logic test binaries on macOS | `just ci` on arm64 and Intel | Linux per PR (`just ci`); suite=all `just test` on both Macs; `ci-release` on the Mac |
| Intel macOS check | every PR (`just ci`, harnesses, drop-in) | arm64's cross `cargo check` per PR; the whole job in suite=all |
| deny, notices (and cargo-about), fmt, exporter lock, workflow test, release-deps on macOS | `just ci` on both Macs | Linux per PR only (identical on every platform) |
| clippy on Intel | `just ci` | Linux and arm64 per PR |
| fidelity on macOS | both Macs | Linux per PR; `ci-release` (full, with perf) |
| netsim soak | linux-check per PR (`just netsim-soak`, ignored test) | `soak_` test: `test-release` (`ci-release`, linux-check suite=all `full-scale`) |
| macOS reduced forward soak (`forward-soak 32 2000`) | both Macs | deleted (smoke `forward` covers its sizes; `forward-soak` at full size runs in `test-release`) |
| channel_churn 20,000 x2 | every `cargo test` | 2,000 per PR; 20,000 in `test-release` |
| fil-forward `forward` 8 MiB / 2,000 / 1,000 QUIC concurrent / 30 epochs | every `cargo test` | smoke sizes per PR; full sizes in `test-release` (`forward-soak`) |
| fil-predict, status, MUX, keyless, SOCKS at soak sizes | on demand (`predict-soak`, `status-soak`, `mux-soak`, `forward-soak`) | `test-release` at full scale (the recipes remain) |
| drop-in `idle`, `fwd_churn`, `fwd_bulk_typing`, prediction scenarios but `predict_secret`, 45 s sleep variants | both Macs per PR | macos-check suite=all (both Macs); `ci-release` |
| fat-LTO no-test-hooks build | both Macs per PR | `ci` profile per PR (Linux, and arm64 through `dropin`); fat LTO in suite=all and `ci-release` |
| fil-client `review.rs` (PR #17 probes) | every `cargo test` | folded into `offline.rs`, `status.rs`, `prompts.rs`, `swap.rs`; two were already covered (`swap.rs`, `resize_order.rs`) |
| wall-clock tests | (not per PR since 2026-11-09) | `ci-release` (`test-wallclock`) |

**Security stays in every PR.** Only scale moved out: the §11 hostile-daemon and flood tests,
peer-UID and socket-directory checks, the agent socket, the §11.1 privacy logs (`log_content`,
`ports_log`), the secret-rule tests (`predict_stack`, `predict_e2e`, `predict_suites`'s CI cells,
`review_attacks`, `keyless_reads`), the guard sweep and simulator guard proofs, `remote_commands`,
the install and trampoline contracts, `tty_group`, release-deps and no-test-hooks, and the
drop-in suite's `predict_secret` all run on every PR (Linux, and arm64 for the OS ones).
