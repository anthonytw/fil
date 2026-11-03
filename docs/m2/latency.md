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

Run: [41640812186](https://github.com/anthonytw/fil/actions/runs/41640812186), 2026-11-03 (ET), on `m2-l1` at `c0a2721` (all four scenarios passed; an earlier run, 41639007590, on the previous commit agreed with it to about 1 ms in every cell, apart from `l_catchup` failing there on a harness bug). Runner: GitHub `ubuntu-24.04`, OpenSSH 9.6p1 for the client and the sshd, a single run per cell (300 to 1000 keys each), so read differences under 1 to 2 ms as noise. No key was lost in any cell.

### 1. `TCP_NODELAY`

| Case | client sets it | sshd sets it | may Nagle |
|---|---|---|---|
| `ssh -T` | yes | yes | no |
| `ssh -tt` | yes | yes | no |
| the `ssh` fil starts (client side) | yes | not traced (it is the `-T` case) | no |

OpenSSH 9.6 sets `TCP_NODELAY` on the connection whether or not there is a pty. The only difference in the traces is `IP_TOS`: 0x08 (throughput) for `-T` and 0x10 (low delay) for `-tt`, which does nothing on this path. The claim in the plan, that `ssh -T` may Nagle, does not hold for this OpenSSH. The raw traces are in the artifact (`run/lat-strace-*.txt`).

### 2. Keystroke-to-echo, idle, 40 ms round trip (20 ms each way, +-5 ms jitter in each direction) (ms, p50 / p99)

| Path | 5 keys/s | 20 keys/s | 50 keys/s | 20 keys/s, no netem |
|---|---|---|---|---|
| `ssh -tt` | 52.04 / 64.93 | 50.05 / 64.93 | 50.45 / 64.73 | 10.67 / 21.20 |
| `ssh -tt -o ObscureKeystrokeTiming=no` | n/a | 40.37 / 49.28 | n/a | 0.47 / 0.52 |
| fil, pipe (`ssh -T` underneath) | 42.42 / 49.81 | 41.81 / 49.18 | 41.40 / 49.51 | 0.83 / 0.97 |
| fil, QUIC | 41.45 / 49.05 | 41.34 / 49.10 | 41.03 / 49.43 | 0.70 / 0.81 |

Lost keys: 0 of 300 to 1000 in every cell. fil's p50 is the round trip plus about 1 ms: 41.8 against `sshno`'s 40.4 on the network, and 0.8 against 0.5 with none.

**`ssh -tt` is slow because of `ObscureKeystrokeTiming`, not because of fil.** With no network at all, `ssh -tt` takes 10.7 ms at p50 and 21.2 ms at p99 for an echo that fil's pipe answers in 0.8 ms. That is OpenSSH 9.5 and later sending an interactive session's keystrokes in packets at fixed 20 ms intervals (the median wait is half an interval, the p99 nearly a whole one). Turning it off with `-o ObscureKeystrokeTiming=no` brings `ssh -tt` to 0.47 ms with no network and 40.4 ms on it, so the option costs 9.7 ms at p50. It only applies to sessions with a pty, so the pipe (`ssh -T`) never pays it. A fil that adopted `ssh -tt` would pay it unless it also passed that option.

### 3. Keystrokes under a bulk transfer through `ssh -L`, 20 keys/s, 20 Mbit/s each way, 500-packet queue (ms)

| Load | bulk Mbit/s | p50 | p99 | p50 / idle | H1 rule exceeded (> 2x idle + 10 ms) |
|---|---|---|---|---|---|
| idle | none | 49.93 | 66.24 | 1.0x | no |
| bulk up (client to server) | 20.0 | 851.75 | 1142.18 | 17.1x | yes (110 ms) |
| bulk down (server to client) | 20.0 | 844.67 | 1113.82 | 16.9x | yes (110 ms) |

The bulk fills the 500-packet queue of the direction it loads (about 300 ms at 20 Mbit/s), and `ssh -L` shares one TCP connection between the typing and the bulk data, so the keystroke or its echo waits behind that queue and ssh's own buffers whichever way the bulk goes: about 850 ms, 8 times the bar H1 sets. The idle row has `ssh -tt`'s obscure timing in it (50 ms); it is the same ssh the loaded rows were made with.

### 4. Catch-up with a fast discarding consumer, `cat` of 4.0 MB (55000 lines), runs with a `fil: skipped` marker

| Path | Network | Realnet (5 runs) | pi1 (20) | pi2 (20) |
|---|---|---|---|---|
| `ssh -tt` (control) | clean | 0/5 (55000 lines every run) | n/a | n/a |
| `ssh -tt` (control) | 40 ms | 0/5 (55000 lines every run) | n/a | n/a |
| fil, pipe | clean | **5/5**, 8 markers | not run | not run |
| fil, pipe | 40 ms | **5/5**, 5 markers | n/a | n/a |
| fil, QUIC | clean | **5/5**, 5 markers | not run | not run |
| fil, QUIC | 40 ms | **5/5**, 10 markers | n/a | n/a |

Catch-up fires on every `cat` of a file this size, on both transports, even with a consumer that parses nothing (the daemon's threshold is 1 MiB behind or 2 s, DESIGN.md §6.2; the file is four times that). `ssh -tt` delivers every line in under 0.8 s, so the consumer is not what holds fil back. The `LINE-` tally of a fil run (44.7k to 51.8k of 55k on the pipe and on QUIC on the clean network, 48.8k to 49.2k on the pipe at 40 ms) counts the snapshot's re-rendered history too, so it is not "lines kept". On QUIC at 40 ms the first run skipped almost everything (174 KB read, two markers) and the next four read 2.6 to 3.1 MB with two markers each. The per-run times (clean network, pipe) were 158 to 173 ms against ssh's 100 to 107, and at 40 ms 478 to 1146 ms against 412 to 722. The Pi half (20 runs per transport on each Pi) is the procedure above; it was not run in this package.

### Recommendation for OD-10

**Do not adopt the NODELAY remedy. Keep the pipe as it is, `ssh -T`.** OD-10 closes without a code change.

1. Step 1 of the rule: both ends of the connection `fil` starts set `TCP_NODELAY`. There is no Nagle to remedy.
2. Step 2: fil's pipe costs 1.4 ms over `sshno` on the network (41.81 against 40.37 at 20 keys/s) and 0.4 ms with none, and sits within 0.5 ms of QUIC (41.81 against 41.34). QUIC has no Nagle by construction, so the most the remedy could gain is about 0.5 ms, against the 20 ms the plan asks for.
3. `ssh -tt` would lose: it costs 8.2 ms more at p50 and 15.8 ms more at p99 than the pipe on this network, 9.7 ms of it ObscureKeystrokeTiming, besides the costs the plan lists (a pty, utmp entries, stderr merged into the stream). If a later change ever needs `-tt`, it must pass `-o ObscureKeystrokeTiming=no`.

### Thresholds for H1

H1 stays conditional on V1's `f_fwd_vs_ssh`. What this run fixes:

- **The trigger** is the plan's rule, with fil's own idle numbers on this network at 20 keys/s: a keystroke p50 under a bulk forward above **2 x idle + 10 ms**, that is, above **93.6 ms** on the pipe (idle 41.81) and **92.7 ms** on QUIC (idle 41.34).
- **The comparison bar** for `f_fwd_vs_ssh`: `ssh -L` on the same network and the same 20 Mbit/s, 500-packet bottleneck gives p50 **852 ms** (up) and **845 ms** (down) and p99 **1142 ms** and **1114 ms**. fil's target, from the plan, is keystroke p99 no worse than that, and the trigger above is a far tighter test, so the real bar is the trigger.
- **Expect H1 to be needed unless forwards are paced.** `ssh -L` exceeds the rule 8-fold. On the pipe, fil's forwards share ssh's TCP connection and its buffers exactly as `ssh -L` does, so V1 should expect the same queueing there; on QUIC the channels share one priority (§8.5), and the bottleneck's queue would fill the same way. A forward that is not held back by a delay-based cap will measure like these two rows.
- **The conditions must be kept** when V1 repeats it: 20 keys/s, `delay 20ms 5ms rate 20mbit limit 500` each way, an upload and a download, the same `keyprobe --shell` echo, the link checked as loaded (at least 2 Mbit/s of bulk).
