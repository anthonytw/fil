# The latency lab (M2 package L1)

Four measurements that decide two things in `docs/m2/plan.md`:

- **OD-10:** is there a Nagle problem on the pipe (`TCP_NODELAY` is set by OpenSSH only for sessions with a terminal, so `ssh -T` may delay small writes), and is the remedy (`ssh -tt` with the relay setting raw mode before the preamble) worth its costs (a pty, utmp entries, stderr merged into the stream)? The plan's rule: avoid `-tt` unless the keystroke p50 improves by more than 20 ms.
- **H1:** the thresholds at which forward pacing is needed. The plan's trigger: keystroke p50 under a bulk forward above 2x idle plus 10 ms, or `ssh -T` Nagling, and the owner approves.

It is one batched realnet run. Nothing here changes fil.

## Running it

On the GitHub runner (the coordinator's route):

```bash
gh workflow run linux-check --ref BRANCH -f ref=BRANCH -f suite=latency
```

`suite=latency` runs one job, `realnet (latency)`, and nothing else: not the `check` job, and not the three default realnet parts. `suite=all` and `suite=realnet` do not include it. The job uploads the artifact `realnet-latency`, and its summary has two tables: the scenario results and the keystroke cells.

On a Linux box with root, Docker not needed, `iproute2`, `iptables`, `conntrack`, `ethtool`, `strace`, `python3` and `openssh-server`:

```bash
just realnet latency                  # all four scenarios
just realnet latency l_nodelay        # one
```

It cannot run on macOS or without root (network namespaces, `tc netem`). The scenarios are `l_nodelay`, `l_keys_idle`, `l_keys_bulk` and `l_catchup`.

**What to keep from the artifact** (everything is under `target/realnet`):

| File | What |
|---|---|
| `results-latency.tsv` | the scenario table: result, seconds, and every measurement as `key=value` |
| `results-latency-cells.tsv` | one row per keystroke cell: transport, load, keys/s, sent, echoed, lost, min, p50, p90, p99, max (ms) |
| `run/l_nodelay-*`, `run/lat-strace-*.txt` | the raw `strace` output of both ssh ends and of the real fil pipe |
| `run/SCENARIO-TAG.keyprobe`, `.drain` | each `keyprobe` result line |
| `run/SCENARIO-TAG.clientlog` | the fil client's debug log of each fil cell (what the path check read) |
| `logs/` | the per-scenario logs |

## The topology

The `udp` part's, unchanged (`docs/m1/realnet.md`, "Direct: network namespaces"): client, router and server namespaces, NAT on the router, `tc netem` on its three egress interfaces so every packet crosses exactly one instance, and the runner's own `sshd` in the server namespace. The client is a throwaway user (`rnclient`), the server account `rnfil`, both made for the run and removed at its end. `latency.sh` sources `udp.sh` for all of it.

The network of a cell, unless it says otherwise: **one-way delay 20 ms with 5 ms of jitter, so a round trip of 40 ms with up to +-10 ms of spread (+-5 ms in each direction)** (`LAT_DELAY_MS`, `LAT_JITTER_MS`), at `rate 1gbit`. The `rate` keyword is what keeps netem's jitter from reordering packets (`s_netem_reorder_probe` shows that plain jitter does reorder, and a reordering network would measure TCP's reaction to reordering, not Nagle). The bulk scenario replaces the rate with a bottleneck, described below.

## The tool: `keyprobe`

`crates/fil-testbins/src/bin/keyprobe.rs`, a static test program like the rest of `fil-testbins`. It runs a command on a pty (30 rows by 100 columns) and has two modes.

**`keyprobe keys`** types one key every 1/rate seconds, on a fixed schedule: the n-th key is due at start + n/rate however late the previous echo was, so a slow path cannot slow the typing down (it would hide the queueing it is meant to show). Each key is timestamped just before it is written to the pty. A reader thread timestamps every chunk of output as soon as the read returns. The keys are 62 symbols in turn (`a`-`z`, `A`-`Z`, `0`-`9`). Echoes come back in order, so a byte that equals the next expected key is its echo; a byte that equals a later key's symbol means the keys in between got no echo, and they are `lost` (one lost echo costs one key, not the rest of the run); any other byte is counted (`unexpected`) and skipped. Percentiles are over the keys that came back, so they are optimistic when `lost` is not 0; `lost` is always printed next to them. The first 10 keys are warm-up and left out.

With `--shell`, the command is an interactive shell (`ssh -tt srv`, `fil srv name`), and `keyprobe` types

```text
stty raw -echo; echo KP-READY-$((6*7)); exec cat
```

and waits for `KP-READY-42`. (The typed text holds the arithmetic and never the result, so the shell's own echo of the line cannot be mistaken for it.) From then on every echo is made by `cat` on the remote host, so each is one round trip through the whole path, from the pty, through the client, the transport and the daemon or sshd, the remote pty and back, as an editor's echo is. It does not measure the tty's own echo (`-echo`), which would be answered by the remote kernel just the same but is not what editors do.

The line it prints:

```text
KEYPROBE rate=20 warmup=10 sent=400 echoed=400 lost=0 unexpected=0 first_key_ms=EPOCH min_ms=.. p50_ms=.. p90_ms=.. p99_ms=.. max_ms=.. mean_ms=..
```

Percentiles are nearest-rank. The harness reads it with `kv`, like every other program's `KEY k=v` line.

**`keyprobe drain`** is the "fast discarding consumer" of measurement 4: it types a line, then reads and throws away everything until an end text appears, counting bytes and occurrences of two texts (the catch-up marker `fil: skipped`, and a tally text) without parsing the output (no terminal emulator, unlike `ptydrive`, whose `vt100` makes it a slower consumer than a real terminal).

Local tests (`cargo test -p fil-testbins --test keyprobe`, part of `just ci`): a raw-mode `cat` as the echo, the `--shell` setup into `sh`, a program that never gets ready, usage errors, and `drain` counting bytes, markers and ends over repeated runs, plus unit tests for the matcher, the percentiles and the split-pattern counter.

## Measurement 1: is `TCP_NODELAY` set? (`l_nodelay`)

A second `sshd` (same configuration, port 2222, no pid file) runs in the server namespace under `strace -f -e trace=setsockopt`. For each of `ssh -T` (no pty) and `ssh -tt` (a pty) the client runs under `strace -f -e trace=setsockopt` too, with `sleep 1` as the command, so both ends' socket options are recorded for the same connection. The scenario counts the successful `setsockopt(..., TCP_NODELAY, [1], ...)` calls on each end (`ssh_T_client_nodelay`, `sshd_T_nodelay`, `ssh_tt_client_nodelay`, `sshd_tt_nodelay`) and notes `ssh_T_may_nagle` and `ssh_tt_may_nagle`: yes if either end did not set it. The keystrokes travel client to sshd (the client's socket decides) and the echo back (sshd's), so each end matters for one direction.

A third trace is of the real thing: `fil --transport pipe` itself, under `strace`, so the `ssh` that fil starts (`ssh -T -- HOST fil server --stdio`, DESIGN.md section 8.2) is the one examined (`fil_pipe_client_nodelay`, client side only). The raw files are in `run/lat-strace-*.txt`, with the other `setsockopt` calls (`IP_TOS` and the like) in them.

## Measurement 2: keystroke latency, idle (`l_keys_idle`)

Three paths, three typing rates, the 40 ms network:

| Path | Command |
|---|---|
| `ssh` | `ssh -tt srv`: the raw baseline, a terminal session of plain OpenSSH |
| `sshno` | the same with `-o ObscureKeystrokeTiming=no` (20 and no-netem cells only; see the results) |
| `pipe` | `fil --transport pipe srv NAME`: fil over `ssh -T` |
| `quic` | `fil --transport direct srv NAME`: fil over QUIC, after the move from the pipe |

at 5, 20 and 50 keys/s: nine cells. Each types at least 300 keys and at least `LAT_SECS` (20) seconds. For the QUIC cell, `keyprobe` waits 3 s after the shell is ready before the first key, and the cell is checked against the fil client's debug log: `Switched(Direct)` must be logged before the first measured key (the scenario fails otherwise: a QUIC cell on the pipe is not a QUIC number). A pipe cell must never show it. Then the same three paths once more at 20 keys/s with **no netem** (`local` cells): the cost of fil itself, with no network under it. fil's predictive echo is not in this milestone, so every echo here is the remote's.

Each cell ends the fil session with `fil kill`.

## Measurement 3: keystrokes under a bulk transfer, through `ssh -L` (`l_keys_bulk`)

The baseline fil's forwards will be held to. `keyprobe` types at 20 keys/s into `ssh -tt -L 19000:... -L 19001:... srv`, so the typing and the forwarded bulk traffic share **one ssh connection, one TCP connection** (the case the plan calls out, and the reason `ssh -L` makes a shell laggy). In the server namespace a small `python3` program runs as `rnfil`: a sink on loopback port 19100 and a source on 19101, which the two `-L` forwards reach. Three cells:

| Cell | Load |
|---|---|
| `sLidle` | none |
| `sLup` | `head -c 16G /dev/zero` through the forward to the sink (client to server, the direction the keystrokes travel) |
| `sLdown` | `cat` of the forward to the source (server to client, the direction the echoes travel) |

The link is a bottleneck for these three cells (the idle cell too, so the three compare): `tc netem delay 20ms 5ms rate 20mbit limit 500` (`LAT_BULK_RATE`, `LAT_BULK_LIMIT`), a 500 packet queue that holds about 300 ms at 20 Mbit/s. Without a rate and a bounded queue the bulk transfer would never build the queue that is the whole effect. The scenario reads the router's byte counter and fails if the transfer did not move at least 2 Mbit/s (`bulk_mbit`): a cell with an unloaded link is not a result. The queue sizes are a choice, not a measurement of any real uplink; the result is a baseline to compare fil against under the same conditions (V1's `f_fwd_vs_ssh`), not a statement about ssh.

For each load the scenario notes `p50_vs_idle` and applies H1's rule, `h1_rule_exceeded=yes` if p50 under the load is above 2x the idle p50 plus 10 ms. (fil has no forwards yet: the `fil` rows of this table are V1's.)

## Measurement 4: how often catch-up fires (`l_catchup`, and the Pis)

A 4.0 MB file (55000 lines of 73 bytes, `LINE-000001 ...`) is `cat`ted in the session, `LAT_CATCH_RUNS` (5) times per cell, by `keyprobe drain`: the consumer reads at full speed and discards. A run counts as a catch-up run if the output contained the daemon's `fil: skipped` marker (DESIGN.md section 6.2: the subscriber fell more than 1 MiB or 2 s behind, so the daemon jumped to a snapshot). Cells: `ssh -tt` as the control (it must deliver all 55000 `LINE-` tokens in every run; if it does not, the consumer is not fast and the scenario fails), fil on the pipe, fil on QUIC; each on the clean network and on the 40 ms one. The note `catchup.KIND.NET=W/R marks=M ms=...` gives the runs with a marker out of R, the marker total and the time of each run.

Two cautions for reading it. A snapshot re-renders recent history, so the `LINE-` tally of a fil run can exceed the lines "kept" (the Pi suite counts lines on the screen instead). And the consumer is a pty reader that parses nothing: a real terminal emulator is slower, so this is the lower bound on how often catch-up fires on real use.

**On the Pis (not run by this package).** The same measurement from this Mac against the two lab Pis, as input for M3. The Pi side needs no new binary: the shell on the Pi has `seq` and `cat`, and `keyprobe` runs here. Nothing below touches the Pis' gate beyond what `docs/m1/pi.md` already uses.

1. Build the client and the probe in separate invocations (the `fil` that is used must not have the test hooks, `just no-test-hooks target/release/fil`):

   ```bash
   cargo build --release -p fil --bins
   just no-test-hooks target/release/fil
   cargo build --release -p fil-testbins --bin keyprobe
   ```

2. Install the Pi binary as `docs/m1/pi.md` describes (the `aarch64` job's artifact) and set `FIL_PI_TARGETS` the same way. Write a client config with the gate's pipe and the Pi's address for QUIC (placeholders here; the real names stay in your environment):

   ```toml
   [host.pi1]
   pipe = ["ssh", "-o", "BatchMode=yes", "fil-test@pi1.example.net", "fil", "server", "--stdio"]
   transport = "pipe"            # "direct" for the QUIC rows

   [host.pi1.direct]
   address = "pi1.example.net"
   ```

3. For each of `pipe` and `direct`, 20 runs:

   ```bash
   XDG_STATE_HOME=$(mktemp -d) FIL_LOG=debug target/release/keyprobe drain \
     --settle-ms 3000 --repeat 20 --timeout 150 \
     --setup "seq -f 'LINE-%06g 0123456789abcdefghij0123456789abcdefghij0123456789abcdefghij' 1 55000 > /tmp/big.txt; echo KP-READY-\$((6*7))" \
     --ready KP-READY-42 --run 'cat /tmp/big.txt; echo CATEND-$((6*7))' --end CATEND-42 --tally LINE- \
     -- target/release/fil --config pi.toml pi1 latcatch
   ```

   then `fil --config pi.toml kill pi1 latcatch` and remove `/tmp/big.txt` on the Pi. For QUIC, check in `$XDG_STATE_HOME/fil/client.log` that `Switched(Direct)` is there, as the realnet scenario does. Do not run it at the same time as `just pi-e2e` or `just dropin`: the gate's `reset` kills everything the account runs.

4. Record, per Pi and transport: runs with a marker out of 20, the marker total, and the times. `just dropin --hosts pi1 --only flood` is the older view of the same thing (lines kept on the screen, behind `vt100`).

## Reading the results

**OD-10.** Three comparisons decide it, all at 20 and 50 keys/s on the 40 ms network:

1. `strace`: does the `ssh` that fil starts (and the sshd) set `TCP_NODELAY` for `-T`? If both ends set it on every socket, there is no Nagle to remedy and OD-10 closes: keep `ssh -T`.
2. The excess of the pipe over the baseline: `(pipe p50 - ssh -tt p50) - (pipe local p50 - ssh local p50)`, that is, fil's keystroke latency over plain ssh's, minus what fil costs with no network. QUIC's difference is the control: QUIC has no Nagle, so a pipe excess well above QUIC's, with `TCP_NODELAY` missing from step 1, is Nagle.
3. If the excess is above 20 ms at 20 or 50 keys/s *and* step 1 found the missing option on the keystroke direction, recommend the remedy; otherwise do not. The remedy is `ssh -tt`, whose cost is listed in the plan; a smaller excess is not worth a pty and utmp entries.

**H1.** The trigger is the plan's: keystroke p50 under a bulk forward above 2x its idle p50 plus 10 ms. The ssh -L cells give the bar and the shape (up and down); V1 measures fil's own forward and compares it with these on this network and this bottleneck. The target the plan sets for `f_fwd_vs_ssh`: keystroke p99 no worse than `ssh -L`'s under the same load. The rows to copy into H1's thresholds are `sLup` and `sLdown` below.

## Results

Run: GitHub Actions run 41640812186 (`linux-check`, `suite=latency`), 2026-11-03 08:02 ET, branch `m2-l1` at commit `c0a2721`, all four scenarios passed. OpenSSH_9.6p1 (Ubuntu 24.04) on both ends. The network is the one above: round trip 40 ms with up to +-10 ms of spread, no loss. Every cell echoed every key (`lost=0` in all 17 cells).

### 1. `TCP_NODELAY`

| Case | client sets it | sshd sets it | may Nagle |
|---|---|---|---|
| `ssh -T` | yes | yes | no |
| `ssh -tt` | yes | yes | no |
| the `ssh` fil starts (client side) | yes | not traced | no |

Contrary to the plan's claim, OpenSSH 9.6 sets `TCP_NODELAY` on both ends for `-T` too, not only for sessions with a terminal. The one visible difference is `IP_TOS`: `-T` sets 8 (throughput), `-tt` sets 16 (low delay). Netem ignores it, so it does not show in these numbers. This was measured on 9.6 only; the plan's claim may hold for older releases.

### 2. Keystroke-to-echo, idle, 40 ms round trip +-5 ms (ms)

| Path | 5 keys/s p50 | p99 | 20 keys/s p50 | p99 | 50 keys/s p50 | p99 | 20 keys/s, no netem p50 | p99 |
|---|---|---|---|---|---|---|---|---|
| `ssh -tt` | 52.04 | 64.93 | 50.05 | 64.93 | 50.45 | 64.73 | 10.67 | 21.20 |
| `ssh -tt -o ObscureKeystrokeTiming=no` (control) | not run | not run | 40.37 | 49.28 | not run | not run | 0.47 | 0.52 |
| fil, pipe | 42.42 | 49.81 | 41.81 | 49.18 | 41.40 | 49.51 | 0.83 | 0.97 |
| fil, QUIC | 41.45 | 49.05 | 41.34 | 49.10 | 41.03 | 49.43 | 0.70 | 0.81 |

Lost keys in any cell: 0 (of 300 to 1000 per cell).

**`ssh -tt` carries about 10 ms of its own.** OpenSSH 9.5 and later send keystrokes at fixed intervals (`ObscureKeystrokeTiming`, default on, applies to sessions with a terminal). With it on, plain `ssh -tt` is 10.67 ms p50 with no network, where the same command with it off is 0.47 ms. On the 40 ms network the difference is +9.7 ms p50 (50.05 against 40.37) and +15.7 ms p99. It does not touch `ssh -T`: fil's pipe, which runs over `ssh -T`, is 0.83 ms with no network (0.36 ms above `ssh -tt` with the option off) and 1.44 ms above the no-option control on the 40 ms network, with nothing near 10 ms. (That is an inference from fil's pipe cells; `ssh -T` was not run with the option itself.) So the fil-over-ssh p50 beats `ssh -tt` by about 8 ms, but that is the cost of the option in the baseline, not a gain from fil. The honest baseline is the `ObscureKeystrokeTiming=no` row.

**fil's excess over plain ssh**, by OD-10's formula `(path p50 - ssh -tt p50) - (path local p50 - ssh local p50)`, in ms:

| Path | 5 keys/s | 20 keys/s | 50 keys/s | 20 keys/s against the control (option off) |
|---|---|---|---|---|
| fil, pipe | +0.2 | +1.6 | +0.8 | +1.1 |
| fil, QUIC | -0.6 | +1.3 | +0.6 | +0.7 |

The local term is the 20 keys/s no-netem cell for all three rates. The pipe is 0.5 ms (20 keys/s) behind QUIC, which has no Nagle.

### 3. Keystrokes under a bulk transfer through `ssh -L`, 20 keys/s, 20 Mbit/s bottleneck (ms)

| Load | bulk Mbit/s | p50 | p99 | p50 / idle | H1 rule exceeded (> 2x idle + 10 ms = 109.9 ms) |
|---|---|---|---|---|---|
| idle | none | 49.93 | 66.24 | 1.0x | no |
| bulk up (client to server) | 20.0 | 851.75 | 1142.18 | 17.1x | yes |
| bulk down (server to client) | 20.0 | 844.67 | 1113.82 | 16.9x | yes |

Typing and bulk share one ssh connection. The idle row includes `ssh -tt`'s ~10 ms (above), which is small next to the effect. Lost keys: 0. The 845 to 850 ms is far more than the 300 ms the router queue can hold. It is close to what ssh's 2 MiB channel window holds at 20 Mbit/s (about 840 ms), so the keystrokes most likely wait in ssh's and TCP's own buffers, not in the router. That inference was not measured here.

### 4. Catch-up with a fast discarding consumer, `cat` of 4 MB, runs with a `fil: skipped` marker

| Path | Network | Realnet (R = 5) | marker total | run times (ms) | pi1 (20) | pi2 (20) |
|---|---|---|---|---|---|---|
| `ssh -tt` (control) | clean | 0/5 | 0 | 100 to 107 | n/a | n/a |
| `ssh -tt` (control) | 40 ms | 0/5 | 0 | 412 to 722 | n/a | n/a |
| fil, pipe | clean | 5/5 | 8 | 158 to 173 | not run | not run |
| fil, pipe | 40 ms | 5/5 | 5 | 478 to 1146 | n/a | n/a |
| fil, QUIC | clean | 5/5 | 5 | 162 to 179 | not run | not run |
| fil, QUIC | 40 ms | 5/5 | 10 | 210 to 331 | n/a | n/a |

The control delivered all 55000 `LINE-` tokens in every run, so the consumer is fast. Every fil run skipped, with one or two markers per run, and fil's clean-pipe runs delivered 44700 to 51200 of the 55000 `LINE-` tokens (a tally can include snapshot re-renders). One QUIC run on the 40 ms network delivered only 2341. Catch-up is therefore the normal outcome of a 4 MB burst, not a rare one, on both transports. The Pi half of this measurement is written (see above) and has not been run; the pi1 and pi2 columns stay empty until it is.

### Recommendation for OD-10

**Do not adopt the remedy; keep `ssh -T`.** OpenSSH 9.6 sets `TCP_NODELAY` on the client and on sshd for `-T`, and the pipe's keystroke latency is 1.6 ms above plain `ssh -tt` at 20 keys/s (0.8 ms at 50, 0.2 at 5) on the 40 ms network, and 1.1 ms above it with `ObscureKeystrokeTiming=no`. QUIC, which cannot Nagle, is 1.3 ms. The remedy's threshold is 20 ms and both conditions fail (the option is set; the excess is 1/12 of the bar). `ssh -tt` would cost a pty, utmp entries and merged stderr, and for fil it would add the ~10 ms keystroke-timing delay measured above. Close the "pipe Nagle" claim in the plan as not found on OpenSSH 9.6. If a user's ssh is old enough to leave `-T` without `TCP_NODELAY`, that is a separate check; this run has no data on it.

### Thresholds for H1

H1's trigger has two arms; one is settled and one waits for V1.

- **`ssh -T` Nagles:** no (section 1). This arm does not start H1.
- **Keystroke p50 under a bulk forward above 2x idle plus 10 ms:** `ssh -L` exceeds it by a wide margin (17.1x up, 16.9x down, p50 852 and 845 ms against a bar of 109.9 ms) at 20 keys/s, 40 ms round trip, a 20 Mbit/s bottleneck with a 500 packet queue. That is the baseline, not fil's number. fil has no forwards yet, so the arm is decided by V1 (`f_fwd_vs_ssh`) on this same network and bottleneck:

| Condition | Threshold |
|---|---|
| H1 is needed if fil's forward p50 under bulk exceeds | 2x idle + 10 ms: about 93 ms on the pipe and on QUIC (idle p50 41.4 and 41.3 ms), in either direction |
| `f_fwd_vs_ssh` fails if fil's forward p99 under bulk exceeds | `ssh -L`'s: 1142 ms (up), 1114 ms (down) |

H1 also needs the owner's approval (plan, H1). If V1 shows fil's forward under both lines, H1 is not built.
