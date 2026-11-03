# Forwards on the real network (M2 package V1, realnet)

The M2 scenarios of the real-network harness (`docs/m1/realnet.md`): port forwards of the real `fil` binary, with real OpenSSH, real NAT and `tc netem`, in the `udp` part's namespaces. They are the `fwd` part, `tests/realnet/fwd.sh`. The plan's list is `docs/m2/plan.md` §5.3; the Pi half is `docs/m2/dropin.md`.

## Running it

```bash
just realnet fwd                                  # all of them, about 25 minutes
just realnet fwd f_fwd_cable_pull f_fwd_vs_ssh    # named ones
just vm-realnet fwd                               # in the local Lima VM (docs/dev/vm.md), aarch64
gh workflow run linux-check --ref BRANCH -f ref=BRANCH -f suite=realnet   # GitHub, x86_64
```

`fwd` is the fourth part of `just realnet` (and of the workflow's `realnet` matrix, which now runs four jobs in parallel; the job's limit went from 45 to 60 minutes for the cable pull's 5 minutes of outage, twice). It sources `latency.sh` (which sources `udp.sh`) for the topology and the keystroke cells, so it needs what both need: root, `iproute2`, `iptables`, `conntrack`, `ethtool`, `openssh-server`, `python3`. A third throwaway user, `rnmallory`, is made for `f_fwd_other_user` and removed at the end.

Knobs: `FWD_PULL_SHORT` and `FWD_PULL_LONG` (30 and 300 s), `FWD_MANY` (10000), `FWD_STRESS_COUNT` and `FWD_STRESS_PAR` (20000 and 200).

## The pieces

Every forwarded byte stream is `fwdprobe` (`docs/m2/pi.md`) at both ends: a seeded stream whose sha256 either end can compute, and a server that counts its accepts.

- **The pair.** Most scenarios run two transfers at once through one client, `fil -L 18080:127.0.0.1:18000 -R 19200:127.0.0.1:19500 srv SESSION`:
  - an **`-L` download**: `fwdprobe serve 127.0.0.1:18000 source N --rate R` as the server's user in the server namespace (the session's side), read by `fwdprobe connect 127.0.0.1:18080 recv` in the client namespace;
  - an **`-R` upload**: `fwdprobe connect 127.0.0.1:19200 send N --rate R --show-reply` in the server namespace, into `fwdprobe serve 127.0.0.1:19500 sink --reply` in the client namespace.
- **The done-when checks** (`fwd_pair_check`), for each transfer: the sha256 is the seeded stream's (`fwdprobe digest`); **the server at the far end accepted exactly one connection** (a forward that quietly reconnected its upstream would show two); the reader ended on EOF. Then, for the session: the daemon is the same process (its pid), the client still runs and its shell answers, and no forward channel ended with `ResumeGone` (the close reasons are noted from the client log: `Done` is a clean close).
- **A miss is a finding, not a harness failure.** It is recorded with `known_bug` (a `KNOWN-BUG:` note; the scenario shows `PASS*`; `REALNET_STRICT=1`, or the workflow's `strict` input, fails it), so one finding does not hide the rest. A broken precondition fails the scenario: an event that would land after the transfer ended, a disruption the client never noticed (no `link down` in its log), a server that never listened, an unloaded link.
- **The sampler.** Every 100 ms the harness reads `bytes_received` of the client namespace's two sockets (`ss -ti`): the `-L` reader's and the `-R` sink's. From it: how long after each disruption the data moved again (`*_resumed_*`), and the longest stall of each transfer (`max_stall_ms`).
- **A pulled cable** is `ip link set c0 down` in the client namespace, then `up` and the default route put back (taking the link down removes it, as a real reconnect gets it back from DHCP). Everything the client sends meanwhile fails at once with "network unreachable", ssh logins included.

## The scenarios

| Scenario | What happens | What is asserted (findings as KNOWN-BUG) |
|---|---|---|
| `f_fwd_cable_pull` | **The done-when.** The pair (200 MB each way at 8 MB/s) while the cable is pulled for 30 s and, after some data, for 5 min. On `auto` (QUIC, with the pipe as fallback) and on `--transport pipe`. On `auto`, plain `ssh -N -L` carries a third 200 MB download through the same two pulls, as the comparison | both transfers byte-exact with one accept each, EOF at the end; the client logged its link down at each pull (asserted); the daemon untouched; no `ResumeGone`. Measured: how long after the plug each transfer moved again; the largest stall. Noted: what ssh -L did |
| `f_fwd_reconnect` | the pair (60 MB at 2 MB/s) across a reconnect: on `auto` UDP cut 6 s (past `dead_after`, so the link is declared dead and redialled), on the pipe the ssh killed | as above; the link went down (asserted) |
| `f_fwd_swap` | the pair (120 MB) across QUIC → pipe → QUIC: UDP cut until the session is on the pipe, then back until it is on QUIC again | as above; both moves happened (asserted, from the client log) |
| `f_fwd_sleep` | the pair across a client sleep: its process group stopped 12 s on `auto`, 45 s on the pipe | as above; the link went down (asserted) |
| `f_fwd_roam` | the pair across three NAT moves (a new port, another, a new address; conntrack proves each) | as above, and no link down, no transport switch, no ssh. Measured: the largest stall |
| `f_fwd_many_short` | 10,000 sequential and 200 concurrent short connections (1000 bytes echoed each) through the `-L` and through the `-R`, on one QUIC link (clean network) | all echoed; no link down; no switch (M1's link died at the 1,025th stream) |
| `f_fwd_ending_stress` | **the `MAX_ENDING` stress.** 20,000 short connections, 200 at a time, through the `-L` and through the `-R`, on three paths: lossy (`delay 20ms 5ms loss 3%`), slow (`delay 20ms 5ms rate 5mbit`), both | measured: connections a second, failures, link downs, moves to the pipe, and how often a side ended the link as "the peer does not answer stream ends" (client log and daemon log). A trip is a finding, with its numbers |
| `f_fwd_other_user` | another user of the host connects to the session's loopback `-R` port, then to the daemon's socket | the `-R` port answers (documented, as with ssh: a loopback port is open to every local user); the daemon socket refuses (`EACCES`, asserted) |
| `f_fwd_vs_ssh` | **H1's data.** Keystrokes at 20 keys/s (`keyprobe --shell`) under a bulk transfer through the same client's `-L`, idle, upload and download, on `delay 20ms 5ms rate 20mbit limit 500` each way: `ssh -tt -L` (again, on this machine), fil on the pipe, fil on QUIC | the link was loaded (at least 2 Mbit/s of bulk, asserted per cell); the QUIC cells were on QUIC before the first measured key, the pipe cells never (asserted). Measured: p50, p99, bulk Mbit/s; H1's rule; the plan's target against ssh |

The conditions of `f_fwd_vs_ssh` are L1's (`docs/m2/latency.md`, "Thresholds for H1"): 20 keys/s, the same bottleneck each way, an upload and a download, the same echo through `cat` on the remote, the link checked as loaded. Two differences, both to keep the measurement on the steady state: `keyprobe` waits 6 s after the shell is ready before the first key (L1: 0.5 s for ssh, 3 s for QUIC), and the QUIC cells start the bulk only once the client has moved to QUIC (otherwise it starts on the pipe the client attached over). The ssh cells are run again in the same job, so each comparison is between numbers from one machine.

Not here yet, waiting for their packages: `f_agent_ssh_hop` and the agent half of `f_fwd_other_user` (A1, PR #74), `f_fwd_ports_scope` (`fil ports`, F4 part 2, PR #73), `f_first_login_prompts`.

## Results

(Filled in from the runs below.)
