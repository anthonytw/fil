# Forwards on the real network (M2 package V1, realnet)

The M2 scenarios of the real-network harness (`docs/m1/realnet.md`): port forwards of the real `fil` binary, with real OpenSSH, real NAT and `tc netem`, in the `udp` part's namespaces. They are the `fwd` part, `tests/realnet/fwd.sh`. The plan's list is `docs/m2/plan.md` §5.3; the Pi half is `docs/m2/dropin.md`.

## Running it

```bash
just realnet fwd                                  # all of them: about 42 minutes (the stress cells about 20 of them)
just realnet fwd f_fwd_cable_pull f_fwd_vs_ssh    # named ones
just realnet fwd-cable                            # the cable pull alone (about 13 minutes)
just realnet fwd-rest                             # everything else
just realnet agent                                # the agent scenarios (about 3 minutes)
just vm-realnet fwd                               # in the local Lima VM (docs/dev/vm.md), aarch64
gh workflow run linux-check --ref BRANCH -f ref=BRANCH -f suite=realnet   # GitHub, x86_64
```

`fwd` and `agent` are parts of `just realnet` (`all` runs them after ssh, udp, sysd and S1's policy). The workflow's `realnet` matrix runs seven jobs in parallel: `ssh`, `udp`, `sysd`, `policy` (S1), `fwd-cable` (the cable pull, about 16 minutes with the build), `fwd-rest` (the other forward scenarios, about 20 minutes with `FWD_STRESS_COUNT=4000`: at 5,000 the stress showed nothing that 20,000 did not, only its rate) and `agent`. Each job has its own time limit, about twice its usual time. It sources `latency.sh` (which sources `udp.sh`) for the topology and the keystroke cells, so it needs what both need: root, `iproute2`, `iptables`, `conntrack`, `ethtool`, `openssh-server`, `python3`. A third throwaway user, `rnmallory`, is made for `f_fwd_other_user` and removed at the end.

Knobs: `FWD_PULL_SHORT` and `FWD_PULL_LONG` (30 and 300 s), `FWD_MANY` (10000), `FWD_STRESS_COUNT` and `FWD_STRESS_PAR` (20000 and 200; CI sets 4000).

## The pieces

Every forwarded byte stream is `fwdprobe` (`docs/m2/pi.md`) at both ends: a seeded stream whose sha256 either end can compute, and a server that counts its accepts.

- **The pair.** Most scenarios run two transfers at once through one client, `fil -L 18080:127.0.0.1:18000 -R 19200:127.0.0.1:19500 srv SESSION`:
  - an **`-L` download**: `fwdprobe serve 127.0.0.1:18000 source N --rate R` as the server's user in the server namespace (the session's side), read by `fwdprobe connect 127.0.0.1:18080 recv` in the client namespace;
  - an **`-R` upload**: `fwdprobe connect 127.0.0.1:19200 send N --rate R --show-reply` in the server namespace, into `fwdprobe serve 127.0.0.1:19500 sink --reply` in the client namespace.
- **The done-when checks** (`fwd_pair_check`), for each transfer: the sha256 is the seeded stream's (`fwdprobe digest`); **the server at the far end accepted exactly one connection** (a forward that quietly reconnected its upstream would show two); the reader ended on EOF. Then, for the session: the daemon is the same process (its pid), the client still runs and its shell answers, and no forward channel ended with `ResumeGone` (the close reasons are noted from the client log: `Done` is a clean close).
- **A miss fails the scenario.** Every check the done-when or a security property makes (a sha256, the one accept, EOF, the reader's exit, the daemon's pid, `ResumeGone`, the session replaced, the client gone, a refused forward listening, another user reaching a socket, a stress cell's link going down) is a *miss* when it does not hold: `fwd_miss` records it at once (a `MISS:` note and the log) and the scenario goes on, so every miss is visible, then `fwd_verdict` fails it at its end. Each scenario runs in a process of its own, so a failure hides nothing of the next one. `known_bug` (a `KNOWN-BUG:` note, `PASS*`) is only for a **named finding**, which says what fixes it (plan §1.5: no XFAIL without one): none is open: the one there was, the pipe's slow return after a long outage, was fixed by PR #77 (the network watcher for every transport), and its check is now a miss like the others. A broken precondition fails the scenario at once: an event that would land after the transfer ended, a disruption the client never noticed (no `link down` in its log), a server that never listened, an unloaded link, a missing daemon log.
- **The sampler.** Every 100 ms the harness reads `bytes_received` of the client namespace's two sockets (`ss -ti`): the `-L` reader's and the `-R` sink's. From it: how long after each disruption the data moved again (`*_resumed_*`), and the longest stall of each transfer (`max_stall_ms`).
- **A pulled cable** is `ip link set c0 down` in the client namespace, then `up` and the default route put back (taking the link down removes it, as a real reconnect gets it back from DHCP). Everything the client sends meanwhile fails at once with "network unreachable", ssh logins included.

## The scenarios

| Scenario | What happens | What is asserted (each a miss when it fails) |
|---|---|---|
| `f_fwd_cable_pull` | **The done-when.** The pair (200 MB each way at 8 MB/s) while the cable is pulled for 30 s and, after some data, for 5 min. On `auto` (QUIC, with the pipe as fallback) and on `--transport pipe`. On `auto`, plain `ssh -N -L` carries a third 200 MB download through the same two pulls, as the comparison | both transfers byte-exact with one accept each, EOF at the end; the client logged its link down at each pull (asserted); the daemon untouched; no `ResumeGone`; the forwards moving again within 5 s of the plug, on both transports (auto measured 0.6 to 2.3 s before PR #77; the pipe, without a network watcher until PR #77, up to 28 s). Measured, every run: how long after the plug each transfer moved again; the largest stall. Noted: what ssh -L did |
| `f_fwd_reconnect` | the pair (60 MB at 2 MB/s) across a reconnect: on `auto` UDP cut 6 s (past `dead_after`, so the link is declared dead and redialled), on the pipe the ssh killed | as above; the link went down (asserted) |
| `f_fwd_swap` | the pair (120 MB) across QUIC → pipe → QUIC: UDP cut until the session is on the pipe, then back until it is on QUIC again | as above; both moves happened (asserted, from the client log) |
| `f_fwd_sleep` | the pair across a client sleep: its process group stopped 12 s on `auto`, 45 s on the pipe | as above; the link went down (asserted) |
| `f_fwd_roam` | the pair across three NAT moves (a new port, another, a new address; conntrack proves each) | as above, and no link down, no transport switch, no ssh. Measured: the largest stall |
| `f_fwd_many_short` | 10,000 sequential and 200 concurrent short connections (1000 bytes echoed each) through the `-L` and through the `-R`, on one QUIC link (clean network) | all echoed; no link down; no switch (M1's link died at the 1,025th stream) |
| `f_fwd_ending_stress` | **the `MAX_ENDING` stress.** 20,000 short connections, 200 at a time, through the `-L` and through the `-R`, on three paths: lossy (`delay 20ms 5ms loss 3%`), slow (`delay 20ms 5ms rate 5mbit`), both; then 5,000 each way on a harsh one (`delay 40ms 10ms loss 8% rate 2mbit`); then the lossy burst through `ssh -N -L`, for scale. The client logs at `fil=debug,fil_forward=info` here, so 20,000 channels' debug lines do not rotate the link's own lines out of its 4 MiB log | every connection echoed; no link down and no move to the pipe in a cell (a trip ends the link and the channels resume on the next one, so the burst alone would not show it); no side ended the link as "the peer does not answer stream ends" (client log and daemon log; a missing daemon log is an error). Measured: connections a second and the counts |
| `f_fwd_other_user` | another user of the host connects to the session's loopback `-R` port, then to the daemon's socket: as fil left it, then with the directory and the socket opened by hand (as root, restored at once) | the `-R` port answers (noted, documented: as with ssh a loopback port is open to every local user); the daemon socket refuses with `EACCES` (the 0700 directory), and with the door opened the daemon hangs up on the stranger (its peer-UID check: `CLOSED`) while its own user is greeted (`DATA`) |
| `f_fwd_vs_ssh` | **H1's data.** Keystrokes at 20 keys/s (`keyprobe --shell`) under a bulk transfer through the same client's `-L`, idle, upload and download, on `delay 20ms 5ms rate 20mbit limit 500` each way: `ssh -tt -L` (again, on this machine), `ssh -tt -o ObscureKeystrokeTiming=no -L` (the like-for-like baseline: no 20 ms keystroke batching, which fil's pipe never pays), fil on the pipe, fil on QUIC | the link was loaded (at least 2 Mbit/s that the application received, asserted per cell); the QUIC cells were on QUIC before the first measured key, the pipe cells never (asserted). Measured: p50, p99, **goodput** (the bytes the application received: the sink's socket in the server namespace for an upload, the reader's in the client namespace for a download; the router's wire counter, which counts retransmissions too, is noted beside it); H1's rule; the plan's targets against ssh |
| `f_agent_ssh_hop` (`agent` part) | a throwaway `ssh-agent` of the client's user holds a fresh key that sshd accepts for this scenario; `fil -A`; in the session `ssh-add -l`, then a real `ssh rnfil@server` from the session; again after UDP is cut and the session is on the pipe; then the same from a session without `-A` | the key is listed by fingerprint; the hop authenticates over QUIC and after the move to the pipe; without `-A` it is refused (the control: the relayed key is what authenticates) |
| `f_agent_other_user` (`agent` part) | another user connects to the session's agent socket, as fil left it and with its two directories and the socket opened by hand | `EACCES` at the directories; with the door open the relay's peer-UID check hangs up (`CLOSED`) while the session's user is let in (`WAITING`: the agent waits for a request); the directory is `700` and the session's `SSH_AUTH_SOCK` is that socket; after `~.` the socket is gone |

The conditions of `f_fwd_vs_ssh` are L1's (`docs/m2/latency.md`, "Thresholds for H1"): 20 keys/s, the same bottleneck each way, an upload and a download, the same echo through `cat` on the remote, the link checked as loaded. Two differences, both to keep the measurement on the steady state: `keyprobe` waits 6 s after the shell is ready before the first key (L1: 0.5 s for ssh, 3 s for QUIC), and the QUIC cells start the bulk only once the client has moved to QUIC (otherwise it starts on the pipe the client attached over). The ssh cells are run again in the same job, so each comparison is between numbers from one machine.

Not covered here: `f_fwd_ports_scope` (sysd), `f_first_login_prompts`, and the plan's "with `same-user` a `-R` is refused" half of `f_fwd_other_user`: that policy (OD-9) exists only as a seam (`fil-session/src/forward.rs`), not yet as an option.

## Results

Four runs of the part on `m2-v1` (nothing in this package changes fil), all with OpenSSH 9.6p1, all passing:

- GitHub `ubuntu-24.04` x86_64: run 41699600128 (in the pre-recreate repository) (14:03 ET, main at `9988fdd`) and **run 41707448932 (14:53 ET, main at `a6bd109`, this PR's tree: all four realnet parts passed)**;
- the Lima VM on aarch64 (`just vm-realnet fwd`): 13:50 ET (main at `9988fdd`) and 14:48 ET (this PR's tree).

`f_fwd_ending_stress` failed in the two earlier runs on a harness bug (a counter's `grep -c`), fixed before the later two; every other scenario passed in all four. Single runs; read differences under a few percent, or a few hundred ms in a reconnect, as noise. The tables give the earlier GitHub and VM runs; the later ones agree with them (the differences are noted where they matter).

**After the review** (misses fail, goodput, the `sshno` baseline, the agent part; main at `7c5f93a`, A1 merged): GitHub run 41723172416 (in the pre-recreate repository) (16:35 ET) passed all six jobs, `ssh`, `udp`, `sysd`, `fwd-cable`, `fwd-rest` (with `FWD_STRESS_COUNT=4000`) and `agent`, with no miss and no finding (its pipe recovery happened to fall under the bound). The VM (16:38 and 17:11 ET) passed the `agent` part and the `fwd` part with no miss; `f_fwd_cable_pull` was `PASS*`, the named pipe finding (5.4 s and 24.9 s after the plug). The numbers agree with the earlier runs; the H1 table is from GitHub.

### The done-when: a pulled cable (`f_fwd_cable_pull`)

**Both transfers came through byte-exact, with one accept at each server and EOF at the end, on both transports and both machines, across a 30 s and then a 5 min pull.** The client logged its link down at each pull, the daemon was the same process at the end, every forward channel closed `Done` and none `ResumeGone`, and the shell answered afterwards.

| Transport | Machine | 30 s pull: data moving again after the plug | 5 min pull: after the plug | Whole transfer (200 MB each way) |
|---|---|---|---|---|
| auto (QUIC) | GitHub x86_64 | 1.6 s | 1.6 s | 358 s |
| auto (QUIC) | VM aarch64 | 1.9 s | 0.6 s | 391 s |
| pipe | GitHub x86_64 | 9.8 s | **18.3 s** | 383 s |
| pipe | VM aarch64 | 8.0 s | **23.3 s** | 385 s |
| pipe | GitHub, final run | 9.9 s | 8.0 s | |
| pipe | VM, final run | 7.8 s | **28.1 s** | |
| auto | GitHub, after the review | 0.8 s | 1.0 s | |
| pipe | GitHub, after the review | 4.8 s | 1.3 s | |
| auto | VM, after the review | 1.7 s | 2.3 s | |
| pipe | VM, after the review | **5.4 s** | **24.9 s** | |

**Finding, fixed by PR #77: on `--transport pipe` the client came back up to 30 s after the network did.** The scenario records the wait on every run and checks it against 5 s (auto's 0.6 to 2.3 s and a margin) on both transports; until PR #77 the pipe's case was a named finding, and since #77 it is a miss like any other (the numbers after #77 are at the end of this section). The client log of the VM run shows why: the path manager's pipe retry backs off to its cap of 30 s during the outage (`ScheduleRetry(PipeRetry, 30s)` every 30 s from a minute in), and nothing cuts the wait short when the cable is back, because the network watcher that would (`NetworkChanged`) runs only when the setting is not `pipe` (`crates/fil/src/dialer.rs`, `watch_network: setting != TransportSetting::Pipe`: "with the pipe only there is nothing to upgrade to and nobody to hint"). The log has no `NetworkChanged` input at all. On `auto` the watcher sees the default route return and the client is back within 2 s (1.1 to 1.9 s in all four runs). No byte is lost either way; it is a wait, and how long depends on where the retry timer stands when the network returns: 8.0, 18.3, 23.3 and 28.1 s after the 5 min pull in the four runs. Under the 5 s bound the finding shows in most runs; the run after the review happened to catch the retry early (4.8 and 1.3 s), which is the timer's luck, not a fix.

**After PR #77** (main at `6583a2a` merged; the VM, 19:00 ET), with the check a miss on both transports: auto came back 1.8 s after the 30 s pull and 1.0 s after the 5 min one; **the pipe 1.3 s and 2.9 s** (before #77: 5 to 10 s and 8 to 28 s). GitHub x86_64 (run 41745371557, in the pre-recreate repository, all seven realnet jobs passed, `policy` included): auto 1.7 s and 1.6 s, **the pipe 1.2 s and 1.5 s**. All under the 5 s bound, no miss.

**ssh -L, alongside on `auto`'s run: byte-exact, one accept, ssh still running, on both machines (355 s and 354 s).** The kernel kept ssh's TCP connection through 5 min with no route (TCP gives up after about 15 minutes of retransmissions) and the address did not change, so the connection resumed when its next retransmission got through. A cable pull that comes back with the same address and no NAT in between is the case ssh survives; `f_fwd_roam` and the udp part's `f_nat_rebind` are the cases it does not (a new mapping ends ssh's connection, and fil's QUIC link moves). This contradicts the plan's expectation ("ssh is expected to fail") for this exact case, so it is recorded as it is.

### Reconnect, swap, sleep, roam

All byte-exact with one accept each, on both machines. Times are "the data moved again after the event ended" and the transfer's longest stall:

| Scenario | GitHub x86_64 | VM aarch64 |
|---|---|---|
| `f_fwd_reconnect` auto (UDP cut 6 s) | 0.07 s; stall 5.7 s | 0.05 s; 5.8 s |
| `f_fwd_reconnect` pipe (ssh killed) | 1.9 s; 2.0 s | 0.1 s (-L), 1.8 s (-R); 1.8 s |
| `f_fwd_swap` (on the pipe after UDP went; back on QUIC) | 5.5 s; 9.6 s; stall 5.5 s | 5.6 s; 10.4 s; 5.7 s |
| `f_fwd_sleep` auto (12 s) | 0.4 s; 12.4 s | 0.4 s; 12.4 s |
| `f_fwd_sleep` pipe (45 s) | 0.9 s; 45.9 s | 0.9 s; 45.9 s |
| `f_fwd_roam` (three NAT moves) | no link down, no switch; largest stall 148 ms | no link down, no switch; 178 ms |

The stall of a swap is the UDP cut until QUIC is declared dead (`dead_after` 5 s); the transfer then moves on the pipe and back without another stall.

### Many connections (`f_fwd_many_short`, `f_fwd_other_user`)

| | GitHub x86_64 | VM aarch64 |
|---|---|---|
| 10,000 sequential through -L | all echoed, 9.5 s | all echoed, 4.1 s |
| 10,000 sequential through -R | all echoed, 9.5 s | all echoed, 4.2 s |
| 200 concurrent, -L / -R | all echoed, 82 / 81 ms | all echoed, 26 / 32 ms |

One QUIC link throughout: no link down, no switch. Another user (`rnmallory`) reaches the loopback `-R` port (documented: as with ssh, a loopback port is open to every local user). On the daemon's socket it gets `EACCES` from the 0700 directory, and with the directory and socket opened by hand the daemon hangs up on it (`CLOSED`, its peer-UID check) while its own user is greeted (`DATA`), on both machines.

### The agent (`agent` part)

On GitHub x86_64 and in the VM: from the session, a real `ssh rnfil@server` authenticated with a key only the client's throwaway agent held, over QUIC and again after UDP was cut and the session moved to the pipe; from a session without `-A` the same ssh was refused (exit 255). The agent socket is `700`/`600`; another user gets `EACCES` at the directories and, with them opened, the relay's peer-UID check hangs up on it (`CLOSED`) while the session's own user is let in (`WAITING`); the session's `SSH_AUTH_SOCK` is that socket, and after `~.` it is gone.

### The `MAX_ENDING` stress (`f_fwd_ending_stress`)

**It did not trip.** In no cell, either way, did either side end the link as "the peer does not answer stream ends", and no link went down or moved to the pipe. Every connection was echoed. The VM, aarch64 (`just vm-realnet fwd f_fwd_many_short f_fwd_ending_stress`, 14:29 ET; the final VM run agreed within 10 %), 200 connections at a time, 1000 bytes echoed each:

| Path (`tc netem` each way) | Connections | -L: per second | -R: per second | Failed | Trips (client / daemon) |
|---|---|---|---|---|---|
| lossy: `delay 20ms 5ms loss 3%` | 20,000 each way | 141 | 133 | 0 | 0 / 0 |
| slow: `delay 20ms 5ms rate 5mbit` | 20,000 each way | 422 | 418 | 0 | 0 / 0 |
| lossy and slow: `loss 3% rate 5mbit` | 20,000 each way | 125 | 118 | 0 | 0 / 0 |
| harsh: `delay 40ms 10ms loss 8% rate 2mbit` | 5,000 each way | 29 | 27 | 0 | 0 / 0 |

The final GitHub run (x86_64, the same cells) agrees: no trip in any cell; lossy 150 and 144 a second, slow 423 and 417, lossy and slow 119 and 120, harsh 29 and 27; nothing failed; ssh -L on the lossy path 143 a second. After the review, with a cell failing on any trip, failure, link down or move to the pipe: GitHub (4,000 connections a cell, as CI now runs it) and the VM (20,000; lossy 131 and 129 a second, slow 423 and 419, lossy and slow 119 and 114, harsh 31 and 26, ssh -L 135) both passed every cell.

What the numbers say about the concern (C3's review: marker answers ride the control channel while stream credit returns in QUIC frames): at most 200 connections are open at a time, and each one's end marker is answered within a round trip or two of its stream end. Even at 420 connections a second, with every answer delayed by a full second of loss recovery, about 420 ends would be waiting, a fifth of the 2,048 limit. The limit would need an answer delay of about five seconds at that rate, or ten times the concurrency the opener allows (`MAX_OPEN_CHANNELS` is 256). A lossy path makes the rate fall faster than the delay grows: at 3 % loss the connection rate drops to a third, so the product (the ends waiting) goes down, not up.

**A slower measurement worth having (not a bug): a lossy path cuts the connection rate to a third.** On the clean network 200 concurrent connections finish in about 30 ms (`f_fwd_many_short`); on the 3 % lossy path the same burst sustains 135 a second, about 1.5 s per connection with 200 open. Every channel's open, acknowledgement and close travels on the one MUX channel (§8.5), one QUIC stream, so a lost packet there holds up every connection's bookkeeping until it is retransmitted. ssh -L puts it in scale: the same 20,000-connection burst through `ssh -N -L` on the same lossy path ran at **129 a second** in the VM and 143 on GitHub, against fil's 133 to 150. One TCP connection carries all of ssh's channels, so a lost packet holds them all up in the same way; fil is no worse.


### H1's data (`f_fwd_vs_ssh`)

Re-run after the review on GitHub x86_64, run 41723172416 (in the pre-recreate repository) (16:35 ET, this PR's tree with main at `7c5f93a`), with the like-for-like ssh baseline and goodput. Keystroke-to-echo at 20 keys/s, `delay 20ms 5ms rate 20mbit limit 500` each way, the bulk through the same client's `-L`; ms. Goodput is what the application received, Mbit/s; the wire carried 20.0 in every loaded cell:

| Path | idle p50 / p99 | H1 trigger (2x idle + 10) | upload p50 / p99 | download p50 / p99 | goodput up / down | H1 rule |
|---|---|---|---|---|---|---|
| `ssh -tt -L` | 51.3 / 65.1 | 112.5 | 855.4 / 1118.1 | 847.2 / 1097.4 | 19.1 / 19.1 | exceeded, 17x |
| `ssh -tt -o ObscureKeystrokeTiming=no -L` | 40.9 / 49.3 | 91.8 | 854.1 / 1120.8 | 850.7 / 1124.1 | 19.1 / 19.1 | exceeded, 21x |
| fil, pipe | 41.6 / 49.2 | **93.2** | **431.8 / 701.0** | **431.7 / 673.4** | 19.1 / 19.1 | **exceeded, 10.4x** |
| fil, QUIC | 41.8 / 50.1 | **93.6** | **271.1 / 320.2** | **276.5 / 321.6** | 19.0 / 19.0 | **exceeded, 6.6x** |

- **The like-for-like baseline changes the idle row only.** Without OpenSSH's keystroke batching ssh's idle echo is 40.9 ms, the same as fil's 41.6 and 41.8, so its trigger (91.8 ms) is fil's within 2 ms. Under load the batching is irrelevant: 854 ms with or without it.
- **Goodput is the same for all four**, 19.0 to 19.1 Mbit/s of the link's 20 (the rest is TCP, ssh and QUIC framing). No path pays for its latency with throughput, and none retransmits enough to show in goodput. The plan's target ("within 2x ssh's throughput") holds with room to spare.
- **fil beats both ssh baselines on keystroke p99** in every loaded cell: the pipe by about 400 ms and QUIC by about 800 ms.
- **H1's trigger is exceeded on both transports, in both directions.** The pipe's p50 under load is 432 ms against a trigger of 93.2 ms; QUIC's is 271 to 276 ms against 93.6 ms. The earlier runs (below) agree within 1 %, as does L1's ssh -L baseline (852 and 845 ms).

The earlier runs (wire counter, no `sshno` cell): GitHub run 41699600128 gave pipe 432.1 / 688.9 up and 431.7 / 689.8 down, QUIC 273.9 / 320.3 and 272.5 / 320.7, ssh -L 855.0 / 1122.3 and 848.0 / 1108.4. The VM gave the same within 1 % in every loaded cell; its idle rows ran 2 to 8 ms higher (aarch64 under `vz`).

How to read them for OD-10/H1:

- **QUIC: about 273 ms, almost exactly the bottleneck queue.** The 500-packet queue at 20 Mbit/s holds about 300 ms; the keystroke rides its own stream but the same congestion window, so each one waits behind the queue the bulk keeps full. The p99 is tight (320 ms): nothing but the router's queue is in the way. A forward that is paced to keep the queue short (the plan's delay-based cap) is the only thing that would bring this down.
- **The pipe: about 432 ms, the queue plus what sits in TCP and ssh's buffers in front of it.** Half of ssh -L's 850 ms. Most likely because fil's channel window is 1 MiB against ssh's 2 MiB (at 20 Mbit/s, about 420 ms against 840), so less forward data waits in front of a keystroke; that is an inference from the sizes, not a measurement of the buffers. Either way it waits behind the bytes already in the one TCP connection.
- **The idle cells are fil's own cost: none.** 41.6 and 41.8 ms idle against ssh's 40.9 without batching: the network's 40 ms plus a millisecond and a half, as in L1.
