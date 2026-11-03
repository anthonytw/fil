# fil on the lab Raspberry Pis (M1 package P7b)

The M1 scenarios, run on real hardware: this Mac's `fil` client, driven in a pseudo-terminal, against `fil server --stdio` on two aarch64 Debian 13 Raspberry Pi 5s, "mars" and "venus". It is on demand and never part of `just ci`.

## Setup

Each Pi has an unprivileged account, `fil-test`, that the Mac's ssh key reaches through a forced-command gate. The gate allows exactly these verbs; there is no shell and no scp:

| Verb | Does |
|---|---|
| `fil server --stdio` | fil's fixed remote command (DESIGN.md §8.2), exactly |
| `install NAME SHA256 < file` | stores `~/.local/bin/NAME`: verified, atomic, 128 MiB cap |
| `run NAME [ARGS]` | runs an installed binary with plain arguments |
| `status`, `logs [n]` | processes, `/tmp/fil-UID`, installed hashes; the user journal |
| `reset` | kills everything the account runs and clears fil's state |

The account has lingering on and resource caps. The gate is in the sysadmin repo (`ansible/files/fil-test-gate.sh`).

### 1. The aarch64 binary

`aarch64-unknown-linux-musl` is the release target (DESIGN.md §13). It does not build on a Mac: `ring` compiles C and needs an aarch64 musl cross compiler (`aarch64-linux-musl-gcc`), which a stock macOS toolchain lacks. The `aarch64` job in `.github/workflows/linux-check.yml` builds it on a GitHub runner with `cross` (a Docker image that carries the compiler), about four minutes:

```sh
gh workflow run linux-check -f ref=BRANCH -f suite=aarch64
gh run watch                                   # wait for it
gh run download RUN_ID -n fil-aarch64-musl -D out
```

The artifact has `fil` (a static ELF, about 11 MB) and `fil.sha256`.

### 2. Install and check

```sh
SHA=$(cut -d' ' -f1 out/fil.sha256)
ssh -o BatchMode=yes fil-test@HOST install fil "$SHA" < out/fil
ssh -o BatchMode=yes fil-test@HOST run fil --version     # fil 0.0.0
ssh -o BatchMode=yes fil-test@HOST status
```

`FIL_PI_TARGETS` tells the driver which ssh target is which Pi.

## Running the scenarios

```sh
export FIL_PI_TARGETS="mars=fil-test@MARS_HOST,venus=fil-test@VENUS_HOST"
just pi-e2e                                  # both Pis, every scenario (about 4 minutes per Pi)
just pi-e2e --hosts mars --only takeover     # narrow
just pi-e2e --list                           # the scenario names
just pi-e2e --verbose                        # client logs and the Pi's journal
```

The driver is `crates/fil/examples/pi_e2e.rs`:

- It writes a scratch config per host and variant under `target/pi-e2e/` (`FIL_PI_WORK` moves it). The `pipe` is `ssh -o BatchMode=yes fil-test@HOST fil server --stdio`, which is what the gate allows. A name without a `FIL_PI_TARGETS` entry is `fil-test@NAME`, so an ssh alias works.
- It runs the real `target/release/fil` on a pseudo-terminal (`portable-pty`), types into it, and reads the screen with `vt100`: the status line is painted one changed cell at a time, so it can only be read through an emulator. Each client has its own `XDG_STATE_HOME`, so each client log is separate, at `FIL_LOG=debug`.
- Shell output is checked with arithmetic (`echo TAG-$((6*7))` and waiting for `TAG-42`), so the typed echo can never be mistaken for the result.
- Before and after every scenario it ends its own sessions with `fil kill`, then runs the gate's `reset`. It checks at the end that no daemon is left.

Do not run two copies at once. `reset` kills everything the account runs.

## Results

Run on 2026-11-02 (ET), binary sha256 `c2bd1214…`, client built from this branch on macOS aarch64. The Mac is on a different subnet from the Pis, across the home router. All scenarios passed on both Pis. Times are single runs from this network; the two Pis agree within noise.

| Scenario | What the driver checks | mars | venus |
|---|---|---|---|
| attach and detach | attach, run a command, `~.` | first prompt 496 ms (ssh login about 400 ms of it); round trip 7 ms; client gone 6 ms after `~.` | 811 ms; 15 ms; 7 ms |
| reattach | a second client sees the earlier output and the same shell (a shell variable) | earlier output on screen after 163 ms | 280 ms |
| `fil ls`, `detach`, `kill` | `ls` lists the session; `detach` tells the attached client, which exits 1 when it declines to reattach; `kill` ends the client and the session | `ls` 187 ms; detach told after 1.4 s; kill: client exit 129 | 183 ms; 1.0 s; 129 |
| takeover | a second client takes the session; the first is told | second client sees the session in 232 ms; first told 240 ms after the second started, exits 1 | 174 ms; 196 ms; exits 1 |
| killed ssh | `kill -9` the local ssh child mid-session, with input typed right after | same shell back, the typed command ran: 1253 ms; second kill 2191 ms | 1136 ms; 2210 ms |
| daemon restart | client offline, gate `reset`, client may log in again | "is a different session now" prompt 4.6 s after it could log in; after `y` a fresh shell (the old variable gone) | 3.6 s; same |
| lingering | detach, wait 40 s with no login, the shell kept running and its output waited in the session | yes; the shell ran in a `fil session` systemd scope | yes; same |
| direct mode | does it move to QUIC? | **no**: stayed on the pipe | **no**: stayed on the pipe |

### Direct mode did not upgrade from this Mac

With the UDP address set to the Pi's name, the client got the Pi's QUIC port (60000–61000 range) and certificate fingerprint in the attach reply and dialed four times in 20 s, each with "no answer from the daemon within 2s", the backoff growing between tries. The session kept working on the pipe throughout, so the **fallback to the pipe works** (round trip 5 to 7 ms during the failed dials). With the address set to a documentation address nothing answers on, the result is the same.

Why it fails: a UDP probe to mars on ports 9, 40000, 60001, 60500, 60999 and 61001, and to venus on 40000, 60001 and 60500, got silence every time. Ports outside fil's range and ports nothing listens on never produced an ICMP "unreachable" either, so this path drops inbound UDP to the Pis' subnet altogether (the router, between the Mac's subnet and theirs), not just the 60000–61000 range. This confirms the blocked-path half of the plan's "Done when" on real hardware. It does **not** show the other half (moving to QUIC within a second where UDP works) on hardware: that needs a client on the Pis' own subnet, and nothing here gives one. The QUIC path is exercised on loopback by `just local-direct`, and by the realnet suite on Linux (`docs/m1/realnet.md`).

### Gate behavior worth knowing

- **`reset` can take 90 seconds.** It stops the daemon's systemd scope with SIGTERM; an interactive bash ignores SIGTERM, so with a live shell it waits for the unit's 90 s stop timeout, then SIGKILLs. A client attached at the time sees "session killed by signal 9" after about 91 s (observed on mars). End sessions with `fil kill` first, as the driver does, or replace the shell with `exec sleep N`.
- **`reset` restarts the whole systemd user manager** (it kills every process of the account, `systemd --user` included, and lingering starts it again). That is harmless for fil.
- **The `logs` verb shows the user journal**, which has the `Started fil-SESSION-….scope` line for each session (it proves the process-scope path). fil keeps no daemon log file there.

## Findings

Each is reproducible with the steps given. None is fixed here; the coordinator routes them.

1. **`fil detach` reaches the attached client late.** The detached client learns after 1.0 to 1.4 s, not at once, and by a reconnect. Reproduce: attach, then `fil detach HOST SESSION` from another shell. The client log shows `link down` (pipe command exited 0) at the moment of the detach, then a new ssh login after the path manager's 0.8 to 1.0 s pipe backoff, and only then the "detached from elsewhere" prompt. A takeover is told in about 0.2 s over the open link. The user sees "disconnected, reconnecting" before the prompt. The daemon could send the notice before it closes the link.
2. **`docs/m1/binary.md` says `fil kill` from elsewhere gives exit status 137 (SIGKILL); it gives 129.** The client prints `session "…" killed by signal 1 (SIGHUP)`. The kill is SIGHUP first and SIGKILL only if that fails (`crates/fil-session/src/proc.rs`), so the code is deliberate and the doc is stale. Reproduce: `fil kill HOST SESSION` with a client attached and an idle shell.
