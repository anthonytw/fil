# `fwdprobe` on the lab Pis and in realnet (M2 package T1)

The M2 forward scenarios (`docs/m2/plan.md` §5.2 and §5.3) need a TCP peer that can run *inside the fil session* on a Pi, send and check exact bytes, and misbehave on purpose (half-close, RST, never answer). `fwdprobe` is that peer. It is a static binary in `fil-testbins`, with no Python and no other tool needed on the Pi. The Pi setup (the gate, the verbs, `FIL_PI_TARGETS`) is in `docs/m1/pi.md`; nothing here adds a gate verb.

## Getting it onto a Pi

The `install` verb takes any binary: `install NAME SHA256 < file` stores `~/.local/bin/NAME`, verified and atomic. `fwdprobe` goes through it the same way `fil` does:

```sh
SHA=$(cut -d' ' -f1 out/fwdprobe.sha256)
ssh -o BatchMode=yes fil-test@HOST install fwdprobe "$SHA" < out/fwdprobe
ssh -o BatchMode=yes fil-test@HOST run fwdprobe digest 1024      # BYTES=1024 / SHA256=...
```

`run` is only for this smoke check. The scenarios start `fwdprobe` from the shell of a fil session, which is the point: the process then belongs to the session, and `fil ports` can attribute it. Use the full path, `~/.local/bin/fwdprobe`, in case the session's `PATH` lacks `~/.local/bin`.

The binary is the `aarch64-unknown-linux-musl` build of `fil-testbins`. The `aarch64` job in `.github/workflows/linux-check.yml` builds it after `fil` and uploads it, with `fwdprobe.sha256`, in the same `fil-aarch64-musl` artifact:

```sh
gh workflow run linux-check -f ref=BRANCH -f suite=aarch64
gh run download RUN_ID -n fil-aarch64-musl -D out      # fil, fwdprobe and their .sha256 files
```

The job runs `cross build --locked --release -p fil-testbins --bin fwdprobe --target aarch64-unknown-linux-musl` and fails unless `file` reports a static aarch64 ELF. `fil-testbins` pulls in `ring`, so the build needs the same cross compiler as `fil` and does not work on the Mac. A local build (`just pi-build`, plan OD-11) is a possible follow-up; the repo is public now, so Actions minutes are no longer the constraint that made it urgent. For realnet, `tests/realnet/build.sh` builds `fwdprobe` as a static x86_64 musl binary with the other test programs.

For the Mac end (the client side of a `-L`, the server side of a `-R`), build it natively: `cargo build --release -p fil-testbins --bin fwdprobe`.

## What it does

The full reference is the header of `crates/fil-testbins/src/bin/fwdprobe.rs`. In short:

```text
fwdprobe serve ADDR MODE [options]      echo | source N | sink | hold | halfclose | rst
fwdprobe connect ADDR ACTION [options]  recv | send N | hold SECS | burst
fwdprobe digest N [--seed S]            the sha256 of a seeded stream of N bytes
```

- **Seeded streams.** The data is generated (SplitMix64 from a seed), never read from a file, so any size is possible and either end can check the other with just N and the seed. `digest` prints what a stream's sha256 must be.
- **Output is `KEY=v` lines.** A server prints `LISTEN=`, then `ACCEPT=n` as each connection arrives, and a block per finished connection (`CONN=n` ... `CONN_END=n`) with `IN_BYTES`, `IN_SHA256`, `IN_END`, `OUT_BYTES`, `OUT_SHA256`, `OUT_END` and `MS`. `IN_END` is how the receive direction ended: `eof` (a FIN), `reset` (a RST), `timeout`, or `error:KIND`.
- **The accept counter is the `ACCEPT=` lines.** A forward that survives a reconnect shows `ACCEPT=1` and no more; one that quietly reconnected the upstream shows `ACCEPT=2`. With `--max-conns N` the server exits after N connections and prints `ACCEPTS=N`.
- **`--rate BYTES_PER_SEC`** paces a transfer so that a kill, a sleep or a cable pull lands mid-transfer (a 64 MiB loopback copy otherwise ends in a fraction of a second).
- **`connect recv` and `send` wait forever** unless `--timeout SECS` is given (the outage scenarios stall a transfer for minutes); `burst` gives up on a silent connection after 30 s. A paced transfer that falls more than a second behind (a stall) paces from the moment it resumes, so a second interruption still lands mid-transfer.
- A usage error exits 2. `connect` exits 1 when it cannot connect (`CONNECT=refused`, `timeout`, ...) and `burst` exits 1 when any connection failed.

## Using it in the scenarios

Inside the session (`LOG` is a file in the session's working directory):

```sh
~/.local/bin/fwdprobe serve 127.0.0.1:18000 source 67108864 --seed 7 --rate 4000000 > "$LOG" &
```

and on the Mac, through the forward (`-L 18080:127.0.0.1:18000`):

```sh
fwdprobe connect 127.0.0.1:18080 recv      # IN_SHA256= must equal: fwdprobe digest 67108864 --seed 7
```

| Scenario (plan §5.2, §5.3) | Session side | Client side |
|---|---|---|
| `fwd_local_http`, `fwd_local_kill_ssh`, `fwd_local_sleep`, `f_fwd_cable_pull` | `serve ... source N --rate R` | `connect ... recv` (no `--timeout`: it must outlast the stop or the outage; the 45 s sleep and the 5 min cable pull end in a result, not a timeout); compare `IN_SHA256` with `digest`; `grep -c ^ACCEPT=` on the log is 1 |
| `fwd_many_short`, `fwd_parallel` | `serve ... echo` | `connect ... burst --count 10000` / `--count 200 --parallel 200`; `FAILED=0` |
| `fwd_halfclose` | `serve ... halfclose --response N` | `connect ... send M`: the request is shut down, the response must still arrive in full |
| `fwd_rst` | `serve ... rst` (or `--after-read N`) | `connect ... recv`: `IN_END=reset`, not `eof` |
| `fwd_remote` | on the Mac: `serve ... sink --reply` | in the session: `connect ... send N --show-reply`; the `REPLY=SINK_SHA256=` line equals `OUT_SHA256` |
| `fwd_remote_detached` | none | `connect ... recv` after `~.`: `CONNECT=refused` with a small `CONNECT_MS`, or `IN_END=reset` through the forward |
| `fwd_ports_view` | `serve 0.0.0.0:P hold` and `serve 127.0.0.1:Q hold` | none (read `fil ports`) |
| `fwd_takeover`, `f_fwd_roam` | `serve ... hold` or `source ... --rate R` | `connect ... hold SECS`: `IN_END=timeout` means the connection was still open, `eof` or `reset` means it ended |
| `f_fwd_cable_pull` in realnet | the static x86_64 binary (`tests/realnet/build.sh`) in `rn-server` | in `rn-client` |

For exact examples with every option, read the header of the source file, and `crates/fil-testbins/tests/fwdprobe.rs` for each mode driven over loopback.

## Tests

`cargo test -p fil-testbins --test fwdprobe` drives every mode over loopback (a few seconds): the sha256 of a 3 MiB source, sink, echo and halfclose exchange against an independent model of the generator; the half-close observed as EOF in one direction while the other still carries bytes (both ways round); a RST surfacing as `ConnectionReset`; a hold staying open and then ending; the accept counter and `--max-conns`; `burst` failing against a server that does not echo; `--rate` pacing; a refused connect; and bad usage.
