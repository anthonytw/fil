# Forwards in the drop-in suite (M2 package V1, the Pis)

The M2 half of the drop-in suite (`docs/m1/dropin.md`): this Mac's `fil` client with `-L`, `-R` and `-D`, driven on a pseudo-terminal, against `fil server --stdio` on the two lab Pis through the `fil-test` gate, with `fwdprobe` (`docs/m2/pi.md`) at both ends. The plan's list is `docs/m2/plan.md` §5.2; the real-network half is `docs/m2/realnet.md`.

## Running it

```sh
# once per build: the Pi binaries from the aarch64 job of the same tree (docs/m1/pi.md, docs/m2/pi.md)
for b in fil fwdprobe; do ssh -o BatchMode=yes fil-test@HOST install $b "$(cut -d' ' -f1 out/$b.sha256)" < out/$b; done

export FIL_PI_TARGETS="pi1=fil-test@HOST1,pi2=fil-test@HOST2"
just dropin --hosts pi1 --only fwd_cable_pull,fwd_reconnect     # narrow
just dropin --list | grep fwd_                                  # the forward scenarios
just dropin --local --only fwd_local_http,fwd_parallel          # this machine, a private daemon (a development aid)
```

`just dropin` now also builds this machine's `fwdprobe` (`cargo build --release -p fil-testbins --bin fwdprobe`, in an invocation of its own, so the `fil` it installs keeps no test hooks); the driver finds it next to `--fil`, or takes `--fwdprobe PATH`. The session side runs `~/.local/bin/fwdprobe` from the fil session's shell, as a user's dev server runs there; a host without it fails the scenario as a missing tool.

**Run one driver at a time, and nothing else that binds fil's UDP range here.** The gate's `reset` kills everything the account runs, and the cut relay (below) binds the daemon's whole UDP range (60000 to 61000) on this machine's loopback while a scenario that uses it runs: a second driver's relay, a local fil daemon in direct mode and `just ci`'s daemon tests (`fil-daemon`'s direct-mode tests fail with "direct mode starts") find every port taken meanwhile.

## How a forward scenario checks

The rules of `docs/m1/dropin.md` hold (fresh evidence, exact comparisons, a cleanup before and after each scenario). In addition:

- **Every transfer is a seeded stream** and is compared by sha256 with `fwdprobe digest`.
- **One accept.** The server at the far end counts its connections (`ACCEPT=`). After any disruption it must have seen exactly one: the application kept its TCP connection.
- **Findings are `KNOWN-BUG:` notes.** A miss in what fil must do (a wrong sha256, a second accept, a reset where an EOF belongs, a daemon that changed) is recorded and the scenario goes on; the run reports it as `PASS*` and lists it at the end, and `--strict` makes it a failure. A broken precondition fails the scenario: an event that would land after the transfer ended, a disruption the client never saw (no `link down` in its log), a server that never listened.
- **Files in the session are read through the shell** with a prefix only the shell computes (`sed "s/^/F$((6*7)): /"`), so the echo of the typed command is never read as the file.

### A cable the driver can pull: `netcut.rs`

The gate gives no root, and this Mac's network cannot be cut for a test. So the scenarios that need a real outage put the client's paths through a relay in the driver, and pull *its* cable:

- **The pipe:** the client's ssh goes to `127.0.0.1:PORT` (`-o HostName=127.0.0.1 -o Port=PORT -o HostKeyAlias=HOST`, so the host key is still checked under the host's real name), and the relay connects to the host's port 22.
- **QUIC:** the client's direct address is `127.0.0.1`. The relay binds every port of the daemon's range (60000 to 61000; the daemon picks one at random and announces it) and passes each datagram to the same port of the host, through one upstream socket per client address, and the answers back.
- **Unplugged,** datagrams are dropped both ways; the bytes of a TCP connection are held and delivered when the cable is back (what TCP's retransmissions do across a real pull); a new TCP connection is closed at once, as a connect fails on a down interface. `set_udp` alone is a UDP outage, which moves a session to the pipe.

The client cannot tell the relay from the network: it sees its link go silent, declares it dead, and its redials fail until the cable is back. What a relay does not model is a new address on the client's side (the realnet NAT scenarios do that) and a kernel that tells the client the interface went down (realnet's `ip link set c0 down` does).

## The scenarios

| Scenario | What a person does | What is asserted (findings as KNOWN-BUG) |
|---|---|---|
| `fwd_cable_pull` | **the done-when.** An `-L` download and an `-R` upload (200 MB each at 8 MB/s) while the cable is pulled 30 s and then 5 min, on `auto` (QUIC) and on the pipe | both byte-exact, one accept each, EOF; the client saw its link down at each pull (asserted); the daemon is the same process; no `ResumeGone`; the shell answers. Measured: how soon after the plug the shell answered |
| `fwd_reconnect` | the same pair (60 MB at 2 MB/s) across a reconnect: the ssh killed on the pipe, the cable out 6 s on `auto` (past `dead_after`) | as above (this covers the plan's `fwd_local_kill_ssh`, for `-R` too) |
| `fwd_swap` | the pair (120 MB) across QUIC → pipe → QUIC: UDP out until the session is on the pipe, then back until it is on QUIC | as above; both moves happened (asserted) |
| `fwd_sleep` | the pair across a laptop sleep: the client and its ssh stopped 12 s (`auto`) and 45 s (pipe) | as above (the plan's `fwd_local_sleep`, for `-R` too) |
| `fwd_local_http` | `-L`; 64 MiB from a server in the session | sha256 equal, one accept; `fil ports --json` from this machine lists the forward with at least 64 MiB down. Measured: MB/s |
| `fwd_many_short` | 10,000 sequential short connections through `-L` over QUIC | all echoed; no link down, no switch (M1's link died at 1,024) |
| `fwd_parallel` | 200 concurrent connections through `-L` and through `-R` | all echoed exactly |
| `fwd_halfclose` | the client shuts down its write side (both directions) | the server sees EOF after exactly the request; the response arrives exactly |
| `fwd_rst` | the server resets (both directions) | the client sees a reset, not an EOF |
| `fwd_remote` | `-R`; a client in the session sends 16 MiB to a server here; then a connection opened 8 s into a 15 s outage on the pipe (the client has seen its link down) | exact, one accept; the late connection waits and goes through, exact, within `open_wait` (30 s) |
| `fwd_remote_detached` | `~.`, then five connects to the `-R` port from another session | each refused or reset, the slowest under 100 ms |
| `fwd_dynamic` | `-D`; `curl --socks5-hostname` to `localhost:PORT`, a server on the host's loopback that nothing here listens for | exact, one accept: the name was resolved and connected on the host |
| `fwd_inside_add_rm` | `fil forward -L` from inside the session, answered `n`; then another, answered `y`; then `fil forward --rm` | the client asks (asserted); after `n` nothing listens here (asserted; the refused entry stays in the table unconsented, and an equal add returns it without a second question, so the `y` is for a spec of its own); after `y` it listens, carries 1 MB exactly, and `fil ports --json` shows it as `origin: request`, `consented: true` (asserted); `--rm` closes the listener (asserted) |
| `fwd_hostile_remote` | inside the session, `fil forward -R 19001:<this Mac's LAN address>:PORT`, a canary listening there | refused (non-zero exit, the daemon's `NotPermitted` message), no question asked, the canary never reached (asserted) |
| `fwd_ports_view` | `fwdprobe serve` on 0.0.0.0 and on 127.0.0.1 from the session's shell, and `python3 -m http.server` if the host has python3 | `fil ports --json` in the session parses; both listeners are the session's, with their pids, the wide one `exposed` (asserted); the text view warns about it; the web server is probed as `http` 200 with its `Server` header as the label (the default probe is a `HEAD`; the page title needs `ports.probe = "full"`) |
| `detach_inside` | `fil detach` with no arguments, in the session | the attached client is told ("detached from elsewhere") and exits 1; the session is still listed, detached (asserted) |
| `fwd_bulk_typing` | types 20 lines into vim idle, then 20 more while an unpaced `-L` download runs (`auto` and pipe) | the file is exact. Measured: the echo of a typed line, p50 and p99, against H1's rule from L1 (p50 above 2x idle + 10 ms) |
| `fwd_churn` | 2,000 short connections to warm up, then two rounds of 20,000 sequential ones, on `auto` and on the pipe | all echoed; the daemon's RSS grows under 16 MiB over the first round and does not grow again by as much over the second (growth that repeats is per connection, a leak; growth that flattens is the allocator), and its descriptor table does not grow (`FDSize` in `/proc/PID/status`: the daemon is not dumpable, so `/proc/PID/fd` is root's; the table doubles past each power of two and never shrinks, so a leak shows); the client's RSS grows under 16 MiB |

Not here yet: `agent_*` (A1, PR #74, still open) and `fwd_takeover`. The M1 "Planned (M2)" list (`docs/m1/dropin.md`) is not covered by this package's first part.

## Results

(Filled in from the runs below.)
