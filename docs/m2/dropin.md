# Forwards in the drop-in suite (M2 package V1, the Pis)

> **Living reference** for the harness it describes; where it and `DESIGN.md` differ, `DESIGN.md` wins.

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
- **A miss fails the scenario.** A miss in what a done-when or a security property requires (a wrong sha256, a second accept, a reset where an EOF belongs, a sender or reader that did not end cleanly, a daemon that changed, a refused forward listening, the canary reached, a detached `-R` reaching this machine, a signature that does not verify, the agent filter letting a change through) is a `MISS:` note: the scenario carries on, so every miss is seen, and the driver fails it at its end with all of them listed, whatever `--allow-*` says. Only a **named finding**, a `KNOWN-BUG:` note that says what fixes it, passes, as `PASS*` (and fails with `--strict`); none is open on the Pis. A broken precondition fails the scenario at once: an event that would land after the transfer ended, a disruption the client never saw (no `link down` in its log), a server that never listened, the daemon's announced UDP port held by another process here.
- **Files in the session are read through the shell** with a prefix only the shell computes (`sed "s/^/F$((6*7)): /"`), so the echo of the typed command is never read as the file.

### A cable the driver can pull: `netcut.rs`

The gate gives no root, and this Mac's network cannot be cut for a test. So the scenarios that need a real outage put the client's paths through a relay in the driver, and pull *its* cable:

- **The pipe:** the client's ssh goes to `127.0.0.1:PORT` (`-o HostName=127.0.0.1 -o Port=PORT -o HostKeyAlias=HOST`, so the host key is still checked under the host's real name), and the relay connects to the host's port 22.
- **QUIC:** the client's direct address is `127.0.0.1`. The relay binds every port of the daemon's range (60000 to 61000; the daemon picks one at random and announces it) and passes each datagram to the same port of the host, through one upstream socket per client address, and the answers back.
- **Unplugged,** datagrams are dropped both ways; the bytes of a TCP connection are held and delivered when the cable is back (what TCP's retransmissions do across a real pull); a new TCP connection is closed at once, as a connect fails on a down interface. `set_udp` alone is a UDP outage, which moves a session to the pipe.

**The relay never sends QUIC to a port it does not hold.** Other work on this Mac may hold a port of the range (another agent's fil daemon in direct mode held one during these runs; `just ci`'s daemon tests bind some too), and the client's QUIC goes to 127.0.0.1 at whatever port the host's daemon announced. So before any QUIC goes through the relay, a pipe-only probe client (it never dials UDP) attaches through it to a session of its own, which starts the daemon, and reads the announced port from its log; unless the relay holds that port (`Cut::covers`), the scenario fails there. The probe's session keeps the daemon, and so the port, until the cleanup after the scenario. The relay holds the range only while a scenario that uses it runs (`fwd_cable_pull`, `fwd_reconnect` on `auto`, `fwd_swap`, `fwd_remote`) and lets it go when the scenario ends. This Mac's ports for the forwards come from the driver's pid, from 30000 to 44999, below macOS's ephemeral range (49152 and up).

The client cannot tell the relay from the network: it sees its link go silent, declares it dead, and its redials fail until the cable is back. What a relay does not model is a new address on the client's side (the realnet NAT scenarios do that) and a kernel that tells the client the interface went down (realnet's `ip link set c0 down` does).

## The scenarios

| Scenario | What a person does | What is asserted (each a miss when it fails) |
|---|---|---|
| `fwd_cable_pull` | **the done-when.** An `-L` download and an `-R` upload (200 MB each at 8 MB/s) while the cable is pulled 30 s and then 5 min, on `auto` (QUIC) and on the pipe | both byte-exact, one accept each, EOF; the client saw its link down at each pull (asserted); the daemon is the same process; no `ResumeGone`; the shell answers. Measured: how soon after the plug the shell answered |
| `fwd_reconnect` | the same pair (60 MB at 2 MB/s) across a reconnect: the ssh killed on the pipe, the cable out 6 s on `auto` (past `dead_after`) | as above (this covers the plan's `fwd_local_kill_ssh`, for `-R` too) |
| `fwd_swap` | the pair (120 MB) across QUIC → pipe → QUIC: UDP out until the session is on the pipe, then back until it is on QUIC | as above; both moves happened (asserted) |
| `fwd_sleep` | the pair across a laptop sleep: the client and its ssh stopped 12 s (`auto`) and 45 s (pipe) | as above (the plan's `fwd_local_sleep`, for `-R` too) |
| `fwd_local_http` | `-L`; 64 MiB from a server in the session | sha256 equal, one accept; `fil ports --json` from this machine lists the forward with at least 64 MiB down. Measured: MB/s |
| `fwd_many_short` | 10,000 sequential short connections through `-L` over QUIC | all echoed; no link down, no switch (M1's link died at 1,024) |
| `fwd_parallel` | 200 concurrent connections through `-L` and through `-R` (100 at a time where either end is macOS, whose kernel caps a listen backlog at 128 and resets what overflows it) | all echoed exactly |
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
| `fwd_churn` | 2,000 short connections to warm up, then two rounds of 20,000 sequential ones, on `auto` and on the pipe | all echoed; the daemon's RSS grows under 16 MiB over the first round and does not grow again by as much over the second (growth that repeats is per connection, a leak; growth that flattens is the allocator), and its descriptor table does not grow (`FDSize` in `/proc/PID/status`: the daemon is not dumpable, so `/proc/PID/fd` is root's; the table doubles past each power of two and never shrinks, so a leak that grows with the connections shows, at 20,000 of them; a slow leak of a few dozen descriptors, below the next power of two, would not); the client's RSS grows under 16 MiB |

The agent scenarios (A1, DESIGN §10) use a throwaway `ssh-agent -D` on this Mac, a child of the driver holding a key made for the run, and the host's own `ssh-add` and `ssh-keygen` (now in the default `--require` list). The client is given that agent's `SSH_AUTH_SOCK`, never this Mac's own. A signature made in the session (`ssh-keygen -Y sign -f k.pub`, which signs with the agent's copy of the key) is verified here with `ssh-keygen -Y verify`, so only the agent's own signature passes.

| Scenario | What a person does | What is asserted (each a miss when it fails) |
|---|---|---|
| `agent_list_sign` | `fil -A`; `ssh-add -l` and `ssh-keygen -Y sign` in the session | the agent's key is listed by fingerprint; the signature verifies here |
| `agent_reconnect` | `-A` on the pipe; signs, the ssh is killed, signs, the client and its ssh sleep 15 s, signs | every signature verifies; `SSH_AUTH_SOCK` is the same path throughout; both disconnects were seen |
| `agent_off_default` | no `-A` | `SSH_AUTH_SOCK` is set (the stable path, OD-6) but the socket does not exist; `ssh-add -l` exits 2 (no agent) |
| `agent_detached` | `-A`, then `~.`; from another session, `ssh-add -l` on the first one's socket | it exits 2, in under 100 ms |
| `agent_filter` | `ssh-add -D` (remove all) and `ssh-add -x` (lock, with an askpass that answers) from the session | both fail; the agent here still holds its key, and a signature still verifies |
| `agent_confirm` | `agent = "confirm"` in the config; two signatures | the overlay asks each time; `n` refuses the signature, `y` signs and it verifies |

Not here yet: `fwd_takeover`. The M1 "Planned (M2)" list (`docs/m1/dropin.md`) is not covered by this package. The M1 "Planned (M2)" list (`docs/m1/dropin.md`) is not covered by this package's first part.

## Results

### The M2 close run (2026-11-04)

Run on 2026-11-04 from this Mac, one Pi at a time, with main at `12f974d` (H1 included) and the Pi binaries of that tree from the `aarch64` job (run 41782753781, in the pre-recreate repository; `fil` sha256 `2d99cde2…`, `fwdprobe` `9d0a2346…`). Each Pi's host policy (`/etc/fil/policy.toml`) allows fil's forwards and the agent for the gate account. **The M2 set (25 scenarios) passed on pi1 and on pi2: 0 failed, 0 skipped, 0 expected failures, no miss, no flake, and nothing refused by policy.** After each run no daemon or socket was left.

- **The done-when holds again, on the build that will ship:** `fwd_cable_pull` moved 200 MB each way through a 30 s and a 5 min outage on both transports and both Pis, byte-exact with one accept each.
- **Recovery after the long outage is slow, as in the first run:** the shell answered 13.9 to 26.6 s after a 5 min pull ended (pi1: 13.9 s on `auto`, 26.6 s on the pipe; pi2: 18.7 and 19.4 s), and 6.5 to 8.6 s after a 30 s pull. The Pis' relay keeps this Mac's address, so the client's network watcher sees no change and waits for its next retry (see "What the Pis add to realnet", below).
- **Typing under a bulk forward (H1):** vim's echo p50 was 27 to 28 ms under a bulk `-L` download against 13 to 15 ms idle, on both transports and both Pis, so H1's rule (above 2x idle + 10 ms) held (p99 40 to 60 ms).
- **`fwd_churn`:** 2 x 20,000 connections per transport, all echoed. The daemon's RSS grew by 2.7 MB over the first round and then by 0 to 80 KiB over the second, and its descriptor table stayed at 64: a high-water mark, not a leak.
- **The agent scenarios** all passed, `agent_confirm` (each signature asked about, `n` refused, `y` signed) and `agent_filter` (`ssh-add -D` and `-x` refused, the key still held) included.
- **The rest, unchanged:** 10,000 sequential connections at 125 per second with no link down; 200 concurrent connections each way in under 100 ms; `fwd_remote` 16 MiB exact, with a connection opened during an outage going through 11.6 to 12.8 s after it was opened.

### Since S1 (#78): the Pis' gate refuses forwards and the agent

From main at `6583a2a` the daemon mirrors sshd's forwarding settings (DESIGN §11.2, decided). The `fil-test` gate account's sshd config turns forwarding off: on a Pi with the merged build (sha256 `5c76e8a8…`, run 41745399062, in the pre-recreate repository), `-L` is refused ("an own forward was refused") and so is `-A` (`agent forwarding refused by server policy: AllowAgentForwarding no (/etc/ssh/sshd_config.d/20-fil-test.conf:6)`). That is fil doing what it should. From then on the forward and agent scenarios could not run on the Pis until their admin allowed them for that account, and this suite never has root there. The admin did: each Pi now has an `/etc/fil/policy.toml` that allows what the scenarios use (DESIGN §11.2, layer 2), and the 2026-11-04 run below passed with nothing refused by policy. The two runs after this one ("After the review" and "The first run") are from the tree before S1.

### After the review (the record)

Run on 2026-11-03 from 17:40 ET, pi1 then pi2, with the client from this PR's tree (main at `7c5f93a`, A1, merged) and the Pi binary of the same tree from the `aarch64` job (run 41721410062, in the pre-recreate repository, `fil` sha256 `5ba2e891…`). Misses now fail a scenario. **The M2 set (25 scenarios: `tools`, the 14 forward ones, the 4 management ones and the 6 agent ones) passed on both Pis: 0 failed, 0 skipped, no miss and no finding.** After each run no daemon or socket was left. Another agent's fil daemon held one UDP port of the range on this Mac throughout; the relay took the other 1,000, and in every relay scenario the daemon's announced port was among them.

| Scenario | pi1 | pi2 |
|---|---|---|
| `fwd_cable_pull` | auto and pipe: both transfers byte-exact, one accept each, across 30 s and 5 min; shell back 6.9 / 10.8 s (auto) and 6.2 / 21.2 s (pipe) after the plug | the same; 0.3 / 16.6 s and 2.1 / 13.4 s |
| `fwd_reconnect`, `fwd_swap`, `fwd_sleep` | exact, one accept each, daemon the same | the same |
| `fwd_many_short` | 10,000 sequential, all echoed, 160/s, no link down | all, 157/s |
| `fwd_parallel`, `fwd_halfclose`, `fwd_rst`, `fwd_dynamic`, `fwd_local_http` | pass | pass |
| `fwd_remote` | 16 MiB exact; a connection opened during an outage went through 11.6 s after it was opened | pass |
| `fwd_remote_detached` | five connects reset at once; the server here accepted 0 | the same |
| `fwd_bulk_typing` | p50 10 ms idle, 21 (auto) and 30 (pipe) ms loaded; under H1's rule | 13 and 14 ms idle, 26 and 30 ms loaded |
| `fwd_churn` | 2 x 20,000 each transport, all echoed; daemon RSS +2.7 MB, then +32 KiB (auto) and +0 (pipe); FDSize 64 throughout | +2.7 MB, then +0 and -32 KiB; 64 |
| `fwd_inside_add_rm` | asked; `n` binds nothing and leaves the entry unconsented; `y` binds, 1 MB exact, consented; `--rm` closes it | the same |
| `fwd_hostile_remote` | refused with the daemon's NotPermitted, no question, canary untouched | the same |
| `fwd_ports_view`, `detach_inside` | pass | pass |
| `agent_list_sign` | the key listed; a signature made in the session verifies here | the same |
| `agent_reconnect` | three signatures (before, after a killed ssh, after a 15 s sleep) verify; the same `SSH_AUTH_SOCK` throughout | the same |
| `agent_off_default` | the stable path set, no socket, `ssh-add -l` exits 2 | the same |
| `agent_detached` | after `~.` `ssh-add -l` exits 2 in 5 ms | the same |
| `agent_filter` | `ssh-add -D` and `-x` refused; the key still held and signing | the same |
| `agent_confirm` | asked twice; `n` refused (exit 255), `y` signed and verified | the same |

### The first run

Run on 2026-11-03 (ET) from this Mac, one Pi at a time. The client is this Mac's, built from `m2-v1` with main at `a6bd109` (F4 part 2) merged; the Pi binary is the `aarch64` job's build of the same tree (run 41700108978, in the pre-recreate repository, `fil` sha256 `e498399a…`, `fwdprobe` `341b7fcd…`), installed through the gate's `install` verb. Single runs over the LAN (about 6 ms round trip), so times are indicative. After each run no daemon or socket was left on either Pi. An earlier run of the transfer scenarios on pi1 with the pre-F4b build (main at `9988fdd`) gave the same results.

**Every forward scenario passes on both Pis, with no finding in fil**, after three fixes to the scenarios themselves (below, "Corrections").

| Scenario | pi1 | pi2 |
|---|---|---|
| `fwd_cable_pull` auto | **byte-exact both ways, 1 accept each**, across 30 s and 5 min; shell back 0.6 s and 17.6 s after the plug; 2 link downs | **the same**; 13.4 s and 13.3 s |
| `fwd_cable_pull` pipe | **byte-exact both ways, 1 accept each**; shell back 8.2 s and 24.8 s after the plug | **the same**; 4.8 s and 19.7 s |
| `fwd_reconnect` | exact, 1 accept each: auto (6 s cut) shell back 2.0 s after; pipe (ssh killed) 1.0 s | exact; 2.0 s, 1.3 s |
| `fwd_swap` | exact; on the pipe 8.2 s after UDP went, back on QUIC 12.0 s after | exact; 8.2 s, 12.4 s |
| `fwd_sleep` | exact; 12 s (auto) and 45 s (pipe), the shell answered at once after the wake | exact; the same |
| `fwd_local_http` | 64 MiB exact, 1 accept, 44 MB/s; `fil ports` shows 67108864 bytes down | exact, 49 MB/s; the same |
| `fwd_many_short` | 10,000 sequential, all echoed, 153/s, no link down | all, 141/s, no link down |
| `fwd_parallel` | 200 concurrent each way, all exact (87 ms, 62 ms) | all exact (58, 47 ms) |
| `fwd_halfclose`, `fwd_rst` | both directions: EOF after the exact request and the response exact; a reset seen as a reset | the same |
| `fwd_remote` | 16 MiB exact, 1 accept; a connection opened 8 s into a 15 s outage went through 13.1 s after it was opened, exact | exact; 12.8 s |
| `fwd_remote_detached` | five connects after `~.`: each reset at once (0 ms) | the same |
| `fwd_dynamic` | curl through SOCKS to the host's `localhost`: 4 MB exact, 1 accept | the same |
| `fwd_bulk_typing` | vim echo p50 11 ms idle, 23 ms (auto) and 26 ms (pipe) under an unpaced bulk download; H1's rule not exceeded | 12 and 13 ms idle, 26 and 33 ms loaded; not exceeded |
| `fwd_churn` | 2 x 20,000 on each transport, all echoed, 117 and 123/s; daemon RSS +2.8 MB over the first round, then +16 KiB on each: a high-water mark, not a leak; descriptor table 64 throughout; client RSS flat | 2 x 20,000 on each transport, all echoed, 118 and 120/s; daemon RSS +2.7 MB over the first round, then +32 KiB (auto) and +0 (pipe): a high-water mark, not a leak; descriptor table 64 throughout; client RSS flat |
| `fwd_inside_add_rm` | asked; `n`: nothing listens here, the entry unconsented; `y`: listening 0.8 s later, 1 MB exact, `origin: request`, consented; `--rm` closed it | asked; `n`: nothing listens here, `fil ports` shows the entry unconsented; `y` (another spec): listening 0.8 s later, 1 MB exact, `origin: request`, consented; `--rm` closed it |
| `fwd_hostile_remote` | refused (exit 1, the same message), no question, canary untouched | refused (exit 1, "a forward added with `fil forward` may not ... connect to a non-loopback target from it (-R)"), no question, canary untouched |
| `fwd_ports_view` | the same as pi2 | both listeners the session's with their pids, the 0.0.0.0 one exposed and warned about; python's server probed as `http` 200, label its `Server` header |
| `detach_inside` | told after 1.1 s, exit 1, the session listed as detached | told after 1.1 s, exit 1, the session listed as detached |

What the Pis add to realnet:

- **The done-when holds on real hardware**: an `-L` download and an `-R` upload, 200 MB each, through a 30 s and a 5 min outage of both paths, byte-exact with one accept each, on both transports and both Pis.
- **Recovery after a long outage is slow on both transports here**, 13 to 25 s after the cable was back. On the Pis the relay keeps this Mac's address, so the client's network watcher sees no change and the client waits for its next retry (backoff up to 30 s) on `auto` too; realnet's `auto`, where the route really disappears and returns, recovers in under 2 s. Realnet's pipe case (no watcher) records it as a finding (`docs/m2/realnet.md`).
- **Typing under a bulk forward on the LAN is fine** (p50 23 to 33 ms against 11 to 13 ms idle, under H1's rule): the LAN has no slow bottleneck for a queue to build in. The realnet numbers are the ones that decide H1.

### Corrections made to the scenarios after the first run

- `fwd_inside_add_rm` first answered `n` and then `y` to the same spec. The refused entry stays in the table, unconsented, and an equal add returns it without asking again (the client asks once per entry, DESIGN §7.3), so the second question never came. The `y` now goes to a spec of its own. Not a bug in fil.
- `fwd_ports_view` first expected the page title as the HTTP label. The default probe is a `HEAD`, which labels with the `Server` header; the title needs `ports.probe = "full"` (`crates/fil-daemon/src/ports/probe.rs`). Not a bug in fil.
- `fwd_churn` first counted `/proc/PID/fd` entries and read 0: the daemon is not dumpable (`hardening.rs`), so the directory is root's. It now reads `FDSize`, and runs a second round to tell growth per connection from the allocator's high-water mark.
