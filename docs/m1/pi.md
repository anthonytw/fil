# fil on the lab Raspberry Pis (M1 package P7b)

The M1 scenarios, run on real hardware: this Mac's `fil` client, driven in a pseudo-terminal, against `fil server --stdio` on two aarch64 Debian 13 Raspberry Pi 5s, called "pi1" and "pi2" below. It is on demand and never part of `just ci`.

## Setup

Each Pi has an unprivileged account, `fil-test`, that the Mac's ssh key reaches through a forced-command gate. The gate allows exactly these verbs; there is no shell and no scp:

| Verb | Does |
|---|---|
| `fil server --stdio` | fil's fixed remote command (DESIGN.md §8.2), exactly |
| `install NAME SHA256 < file` | stores `~/.local/bin/NAME`: verified, atomic, 128 MiB cap |
| `run NAME [ARGS]` | runs an installed binary with plain arguments |
| `status`, `logs [n]` | processes, `/tmp/fil-UID`, installed hashes; the user journal |
| `reset` | kills everything the account runs and clears fil's state |

The account has lingering on and resource caps. The gate script is not part of this repository.

### 1. The aarch64 binary

`aarch64-unknown-linux-musl` is the release target (DESIGN.md §13). It does not build on a Mac: `ring` compiles C and needs an aarch64 musl cross compiler (`aarch64-linux-musl-gcc`), which a stock macOS toolchain lacks. The `aarch64` job in `.github/workflows/linux-check.yml` builds it on a GitHub runner with `cross` (a Docker image that carries the compiler), in two to four minutes:

```sh
gh workflow run linux-check -f ref=BRANCH -f suite=aarch64
gh run watch                                   # wait for it
gh run download RUN_ID -n fil-aarch64-musl -D out
```

The build is reproducible: `cross` is pinned to 0.2.5, its image is pinned by digest in `Cross.toml`, and the build uses `--locked`. The runs before and after the pinning gave the same sha256. The job fails unless `file` reports a static aarch64 ELF. The artifact has `fil` (about 11 MB) and `fil.sha256`, and, since M2 T1, `fwdprobe` and `fwdprobe.sha256` (the TCP test tool, `docs/m2/pi.md`).

### 2. Install and check

```sh
SHA=$(cut -d' ' -f1 out/fil.sha256)
ssh -o BatchMode=yes fil-test@HOST install fil "$SHA" < out/fil
ssh -o BatchMode=yes fil-test@HOST run fil --version     # fil 0.0.0
ssh -o BatchMode=yes fil-test@HOST status
```

## Running the scenarios

```sh
export FIL_PI_TARGETS="pi1=fil-test@HOST1,pi2=fil-test@HOST2"
just pi-e2e                                  # both Pis, every scenario (about 4 minutes per Pi)
just pi-e2e --hosts pi1 --only takeover      # narrow
just pi-e2e --list                           # the scenario names
just pi-e2e --verbose                        # client logs and the Pi's journal
```

`FIL_PI_TARGETS` is required. Its keys are the host labels and the only hosts the driver will touch: it refuses to run without it, rejects a `--hosts` label that is not a key, and never guesses a target. This matters because the gate's `reset` kills everything the account runs. Labels are lower-case letters, digits and dashes; targets are a plain `user@host`. The real host names live only in your environment, never in this repository.

The driver is `crates/fil/examples/pi_e2e.rs`:

- It writes a scratch config per host and variant under `target/pi-e2e/` (`FIL_PI_WORK` moves it). The `pipe` is `ssh -o BatchMode=yes TARGET fil server --stdio`, which is what the gate allows. The UDP address for direct mode is the host part of the target.
- It runs the real `target/release/fil` on a pseudo-terminal (`portable-pty`), types into it, and reads the screen with `vt100`: the status line is painted one changed cell at a time, so it can only be read through an emulator. Each client has its own `XDG_STATE_HOME`, so each client log is separate, at `FIL_LOG=debug`.
- Shell output is checked with arithmetic (`echo TAG-$((6*7))` and waiting for `TAG-42`), so the typed echo can never be mistaken for the result.
- Before and after every scenario it ends its own sessions with `fil kill`, then runs the gate's `reset`. A panic in a scenario is reported as that scenario's failure and the cleanup still runs (a Ctrl-C does not clean up: run `reset` by hand). After the last scenario on a host, a daemon or socket left behind is a failure.

Do not run two copies at once. `reset` kills everything the account runs.

## Results

Run on 2026-11-02 (ET), after the hosts' firewall was opened for UDP 60000–61000 from this machine, on main at a472d35 plus this branch: Pi binary sha256 `ced51e9d…` (from the `aarch64` job, runs 41569420955 and 41571148759), client built from the same tree on macOS aarch64. All scenarios passed on both Pis. Times are single runs from this network; the two Pis agree within noise, except that the daemon restart's wait varies with the client's reconnect backoff (0.5 to 4.5 s). Since UDP is open, the scenarios that attach with the default transport can now move to QUIC right after attaching (which is why the detach time below changed); the ones that need ssh as the transport (killed ssh, daemon restart, lingering) use the pipe only.

| Scenario | What the driver checks | pi1 | pi2 |
|---|---|---|---|
| attach and detach | attach, run a command, `~.` | first prompt 528 ms (the ssh login is about 400 ms of it); round trip 6 ms; client gone 6 ms after `~.` | 586 ms; 6 ms; 5 ms |
| reattach | a second client sees the earlier output and the same shell (a shell variable) | earlier output on screen after 176 ms | 154 ms |
| `fil ls`, `detach`, `kill` | `ls` lists the session; `detach` tells the attached client, which exits 1 when it declines to reattach; `kill` ends the client and the session | `ls` 182 ms; detach told after 186 ms (1.0 to 1.4 s on the pipe before #29, finding 1); kill: client exit 129 | 184 ms; 167 ms; 129 |
| takeover | a second client takes the session; the first is told and exits 1 when it declines to take it back | second client sees the session in 155 ms; first told **within** 186 ms of the second starting (an upper bound: the driver checks the first client after the second one's own round trip) | 179 ms; within 185 ms |
| killed ssh | `kill -9` the local ssh child mid-session, with input typed right after; then a second kill with nothing typed until the client has started a new ssh by itself | same shell back, the typed command ran: 1151 ms; second kill: new ssh after 2.3 s, resumed at 2.5 s | 1054 ms; 2.2 s, 2.3 s |
| daemon restart | client kept offline, gate `reset`, client may log in again; it must show "is a different session now" (a client that exits instead fails the scenario) and a fresh shell must work after `y` | prompt 3.9 s after it could log in; the old shell variable gone | 4.4 s; same |
| lingering | detach, wait 40 s with no login: the daemon is still there and the shell kept running (a background job's output waited in the session); the shell must have run in a `fil session` systemd scope (asserted from the user journal) | yes | yes |
| direct mode | with the default UDP address: moves to QUIC within 1 s of the attach reply, the ssh pipe closes, the session works over QUIC, and the Pi's daemon log shows the direct attach (asserted). With a blackholed UDP address: stays on the pipe and works (asserted) | **yes**: ssh login and attach reply 478 ms after start, QUIC up 26 ms later, carrying the session 2 ms after that: **507 ms** from start; round trip over QUIC 6 ms. Blackhole: pipe, 3 failed dials in 10 s | **yes**: 437 ms, +26 ms, +3 ms: **467 ms**; round trip 6 ms. Blackhole: same |

### Direct mode

**Direct mode works.** With UDP open, `fil` moves from the pipe to QUIC as soon as the attach reply gives it the port and certificate fingerprint. The timeline from the client log, the same on both Pis within noise:

| Step | Time |
|---|---|
| client starts, ssh login, attach reply | about 440 to 480 ms (almost all of it the ssh login) |
| QUIC handshake complete (`DirectUp`) | 26 ms later |
| QUIC carries the session, pipe closes (`Switched(Direct)`) | 2 to 3 ms later |
| start to carrying the session | **467 to 507 ms** |

So the upgrade itself takes about 30 ms after the reply, well inside the plan's "within a second". The pipe's ssh process is gone a moment later, and a command's round trip over QUIC is 6 ms. The Pi's daemon log shows two attaches for the session, the first with `direct: false` (the pipe) and the second with `resuming: true, direct: true`, and `daemon started` names the UDP port it picked in the 60000–61000 range.

With the UDP address set to a documentation address nothing answers on, the client dials three times in 10 s, each with "no answer from the daemon within 2s", and the session stays on the pipe and keeps working (round trip 6 ms). So the **fallback to the pipe works when UDP is blocked from the start**. The driver asserts both cases. If UDP is blocked again, the `direct_mode` scenario fails with a message that says so, rather than passing quietly on the pipe.

**What blocked it earlier.** The first runs, before the hosts' firewall allowed UDP 60000–61000 from this machine's network, stayed on the pipe: four dials in 20 s, each "no answer from the daemon within 2s". A closed-port UDP probe to a Pi got silence on every port, in and outside the range, where an open path returns ICMP "port unreachable". Once the firewall was opened for the admin network, a probe to port 60999 got "unreachable" (the path is open) and 59999 stayed silent (still dropped, as intended). The cause was the hosts' firewall, not the network between.

**Not tested: UDP lost in the middle of a session.** Falling back to the pipe after QUIC has been carrying a session would need UDP blocked from this side while the session runs. That needs a firewall rule (`pf` on the Mac, `ufw` on the Pi), both of which need root, and the gate offers no firewall verb. The path manager's fallback is covered by the netsim and realnet suites (`docs/m1/realnet.md`) and by the dialer tests, not on the Pis.

### Gate behavior worth knowing

- **`reset` can take 90 seconds.** It stops the daemon's systemd scope with SIGTERM; an interactive bash ignores SIGTERM, so with a live shell it waits for the unit's 90 s stop timeout, then SIGKILLs. A client attached at the time sees "session killed by signal 9" after about 91 s (observed once, on pi1). End sessions with `fil kill` first, as the driver does, or replace the shell with `exec sleep N`.
- **`reset` restarts the whole systemd user manager** (it kills every process of the account, `systemd --user` included, and lingering starts it again). That is harmless for fil.
- **The `logs` verb shows the user journal**, which has a `Started fil-HEX.scope - fil session` line for each session (it proves the process-scope path; the session name is deliberately not in it). The daemon's own log lines are in that journal too.

Runs: the `aarch64` job builds, [run 41565928944](https://github.com/anthonytw/fil/actions/runs/41565928944) (first tree), [run 41569420955](https://github.com/anthonytw/fil/actions/runs/41569420955) (merged tree) and [run 41571148759](https://github.com/anthonytw/fil/actions/runs/41571148759) (pinned `cross`, `--locked`, static check: same binary hash). The scenarios in the results table ran against that binary.

## Findings

Each is reproducible with the steps given.

1. **`fil detach` reached the attached client late (fixed in #29).** On the pipe the detached client learned after 1.0 to 1.4 s, by a reconnect, because the daemon closed the link without sending the notice. The daemon now sends `Detach` first (DESIGN.md §5.2), so the client is told at once. The table's 1.0 to 1.4 s is from before the fix.
2. **`docs/m1/binary.md` said `fil kill` from elsewhere gives exit status 137 (SIGKILL); it gives 129 in practice.** The client prints `session "…" killed by signal 1 (SIGHUP)`. The kill is SIGHUP first and SIGKILL only to what survives the grace period (`crates/fil-session/src/proc.rs`), so the code is deliberate and the doc was stale; it has been fixed. Reproduce: `fil kill HOST SESSION` with a client attached and an idle shell.
