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

The build is reproducible: `cross` is pinned to 0.2.5, its image is pinned by digest in `Cross.toml`, and the build uses `--locked`. The runs before and after the pinning gave the same sha256. The job fails unless `file` reports a static aarch64 ELF. The artifact has `fil` (about 11 MB) and `fil.sha256`.

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

Run on 2026-11-02 (ET) on main at a472d35 plus this branch: Pi binary sha256 `ced51e9d…` (from the `aarch64` job, runs 41569420955 and 41571148759), client built from the same tree on macOS aarch64. All scenarios passed on both Pis. Times are single runs from this network; the two Pis agree within noise, except that the daemon restart's wait varies with the client's reconnect backoff (0.5 to 4.5 s).

| Scenario | What the driver checks | pi1 | pi2 |
|---|---|---|---|
| attach and detach | attach, run a command, `~.` | first prompt 530 ms (the ssh login is about 400 ms of it); round trip 6 ms; client gone 6 ms after `~.` | 557 ms; 6 ms; 6 ms |
| reattach | a second client sees the earlier output and the same shell (a shell variable) | earlier output on screen after 143 ms | 176 ms |
| `fil ls`, `detach`, `kill` | `ls` lists the session; `detach` tells the attached client, which exits 1 when it declines to reattach; `kill` ends the client and the session | `ls` 152 ms; detach told after 1.4 s; kill: client exit 129 | 167 ms; 1.0 s; 129 |
| takeover | a second client takes the session; the first is told and exits 1 when it declines to take it back | second client sees the session in 177 ms; first told **within** 190 ms of the second starting (an upper bound: the driver checks the first client after the second one's own round trip) | 162 ms; within 187 ms |
| killed ssh | `kill -9` the local ssh child mid-session, with input typed right after; then a second kill with nothing typed until the client has started a new ssh by itself | same shell back, the typed command ran: 1181 ms; second kill: new ssh after 2.4 s, resumed at 2.5 s | 976 ms; 2.1 s, 2.3 s |
| daemon restart | client kept offline, gate `reset`, client may log in again; it must show "is a different session now" (a client that exits instead fails the scenario) and a fresh shell must work after `y` | prompt 4.5 s after it could log in; the old shell variable gone | 0.5 s; same |
| lingering | detach, wait 40 s with no login: the daemon is still there and the shell kept running (a background job's output waited in the session); the shell must have run in a `fil session` systemd scope (asserted from the user journal) | yes | yes |
| direct mode | does it move to QUIC? | **no**: stayed on the pipe | **no**: stayed on the pipe |

### Direct mode did not upgrade from this Mac

With the UDP address set to the Pi's name, the client got the Pi's QUIC port (60000–61000 range) and certificate fingerprint in the attach reply and dialed four times in 20 s, each with "no answer from the daemon within 2s", the backoff growing between tries. The session kept working on the pipe throughout, so the **fallback to the pipe works** (round trip 6 to 12 ms during the failed dials). With the address set to a documentation address nothing answers on, the result is the same.

Why it fails: a UDP probe to pi1 on ports 9, 40000, 60001, 60500, 60999 and 61001, and to pi2 on 40000, 60001 and 60500, got silence every time. Ports outside fil's range and ports nothing listens on never produced an ICMP "unreachable" either, so this path drops inbound UDP to the Pis altogether (something between this Mac and them), not just the 60000–61000 range. This confirms the blocked-path half of the plan's "Done when" on real hardware. It does **not** show the other half (moving to QUIC within a second where UDP works) on hardware: that needs a client with a UDP path to the Pis, and nothing here gives one. The QUIC path is exercised on loopback by `just local-direct`, and by the realnet suite on Linux (`docs/m1/realnet.md`). That the daemon itself answers on its UDP port on the Pi was not checked.

### Gate behavior worth knowing

- **`reset` can take 90 seconds.** It stops the daemon's systemd scope with SIGTERM; an interactive bash ignores SIGTERM, so with a live shell it waits for the unit's 90 s stop timeout, then SIGKILLs. A client attached at the time sees "session killed by signal 9" after about 91 s (observed once, on pi1). End sessions with `fil kill` first, as the driver does, or replace the shell with `exec sleep N`.
- **`reset` restarts the whole systemd user manager** (it kills every process of the account, `systemd --user` included, and lingering starts it again). That is harmless for fil.
- **The `logs` verb shows the user journal**, which has a `Started fil-SESSION-….scope - fil session SESSION` line for each session (it proves the process-scope path). fil keeps no daemon log file there.

Runs: the `aarch64` job builds, [run 41565928944](https://github.com/anthonytw/fil/actions/runs/41565928944) (first tree), [run 41569420955](https://github.com/anthonytw/fil/actions/runs/41569420955) (merged tree) and [run 41571148759](https://github.com/anthonytw/fil/actions/runs/41571148759) (pinned `cross`, `--locked`, static check: same binary hash).

## Findings

Each is reproducible with the steps given.

1. **`fil detach` reaches the attached client late.** The detached client learns after 1.0 to 1.4 s, not at once, and by a reconnect. Reproduce: attach, then `fil detach HOST SESSION` from another shell. The client log shows `link down` (pipe command exited 0) at the moment of the detach, then a new ssh login after the path manager's 0.8 to 1.0 s pipe backoff, and only then the "detached from elsewhere" prompt. A takeover is told in about 0.2 s over the open link. The user sees "disconnected, reconnecting" before the prompt. The daemon closes the link without sending the notice the client already knows how to show. This is being fixed separately in `fil-session`.
2. **`docs/m1/binary.md` said `fil kill` from elsewhere gives exit status 137 (SIGKILL); it gives 129 in practice.** The client prints `session "…" killed by signal 1 (SIGHUP)`. The kill is SIGHUP first and SIGKILL only to what survives the grace period (`crates/fil-session/src/proc.rs`), so the code is deliberate and the doc was stale; it is fixed in this PR. Reproduce: `fil kill HOST SESSION` with a client attached and an idle shell.
