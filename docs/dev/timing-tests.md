# Wall-clock tests

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
   (`--skip wallclock_`); `just test-wallclock` runs the `wallclock_` ones, one at a time
   (`--test-threads=1`), so they do not measure the contention between tests. `just ci` runs only
   `test` (it is the per-PR check, on shared CI runners whose stalls a wall-clock test would
   measure); `just ci-release` runs both, on a quiet machine before a release, so nothing is left
   out. Name a new wall-clock test with the prefix and it is in the serial step and out of the
   parallel one. Two of `fil_testkit::timing`'s own tests carry the prefix too: they assert that a
   quiet stretch records no stall, which a busy host cannot promise.
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
checks on shared runners at all; `just ci-release` runs them before a release, on the Mac and in
the Linux VM (`just vm-run just test-wallclock`).
