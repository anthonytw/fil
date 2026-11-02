# M1 plan: sessions and both transports

Owner-approved 2026-11-02. M1 in DESIGN.md §15: daemon and sessions; attach, detach and takeover with epochs; resume and snapshot; the pipe and QUIC with upgrade and fallback. Done when fil is usable as a daily driver for shells.

## Done when

- `fil HOST` attaches through ssh, moves to QUIC within a second where UDP works, and stays on the pipe where it doesn't.
- Wi-Fi changes, laptop sleep, a killed ssh and a daemon restart each end in exactly one of these:
  - a seamless resume, with no gap in native scrollback;
  - a snapshot with a visible gap marker;
  - a clear "taken over" or "session replaced" message.
- Attaching from a second machine takes the session over, and the first one is told.
- The real-network suite (`docs/m1/realnet.md`) passes on the real `fil` binary, not stand-ins. It also runs on the lab Raspberry Pis.
- The owner has used it for a few days instead of plain ssh.

## Already merged

| Piece | Where |
|---|---|
| The transport contract: `Link`, `LinkParams`, the in-memory pair | `fil-transport::link` |
| QUIC authentication (pinned daemon certificate, per-attach client key) | `fil-transport::auth`, `docs/m1/quic-auth.md` |
| Frames, control messages, channels, output log, epochs, scheduler, preamble | `fil-proto`, DESIGN.md §8.5 |
| Emulator (alacritty default) and snapshots | `fil-term` |
| Overlay core (for the status line now, prediction in M3) | `fil-predict` |
| Link simulator, real-network harness, stand-in binaries | `fil-netsim`, `tests/realnet`, `fil-testbins` |

## Work packages

Each package is one PR in its own branch. File ownership keeps parallel packages out of each other's way. A package that needs to change something it doesn't own, especially the `Link` trait or the fil-proto wire format, raises it with the coordinator instead of changing it.

### Wave 1 (in parallel)

| Package | Owns | Builds |
|---|---|---|
| **P1: pipe link** | `fil-transport/src/pipe*` | `Link` over any byte stream. **Client side:** it runs the pipe command (`ssh -T HOST fil server --stdio` by default, adding `ServerAliveInterval`), scans the preamble and surfaces the skipped text. **Daemon side:** the relay's Unix socket. Frames go through `fil_proto::sched`. Heartbeats and RTT follow §8.5. Liveness: `Degraded` and `Down(Dead)` on time, including the black hole that ssh alone never notices. |
| **P2: QUIC link and path manager** | `fil-transport/src/quic*`, `fil-transport/src/path*` | **The link:** `Link` over quinn, with the §8.5 stream mapping and `auth` from #9. `Up` comes only after the daemon's first frame. It uses `connect_timeout` (about 2 s) and quinn's keep-alive and idle timeout from `LinkParams`. **Daemon side:** stateless Retry and handshake rate limiting, and the connections a revoked key opened get closed. **The path manager** is pure logic: the client's upgrade from pipe to direct, and its fallback and periodic retry (§4.1–4.2). |
| **P3: sessions** | new crate `fil-session` | One session, transport-agnostic: the PTY and child, the output log, and the alacritty emulator. **Detached behaviour:** query replies while detached, including the OSC 10/11 color queries vim sends. **Attaching:** the epoch state machine wired to its `Effect`s, resume or snapshot (snapshots only at escape-sequence boundaries), and the catch-up rule. **Process scope:** `systemd-run --user --scope` where lingering is on, the process tree otherwise. **Messages for M3:** the PTY's ICANON/ECHO mode, reported in order with the output, and an input-echo ack. Tested over `link::mem`. |
| **P4: client** | new crate `fil-client` | The client, independent of the transport. **Terminal:** raw mode and restore, the escapes (`~.`, `~?`, `~~`), and the one-line status overlay while disconnected (on the `fil-predict` overlay core). **Input while disconnected:** buffer, confirm and log (§4.3). **Attaching:** the `TERM` mismatch warning, and the attach, resume and reconnect loop over a dialer that returns links. **Messages:** clear wording for "taken over", "session replaced" and the shell-noise warning. Tested over `link::mem` against a scripted daemon. |

### Wave 2

| Package | Owns | Builds |
|---|---|---|
| **P5: daemon** | new crate `fil-daemon` | **Socket:** `/tmp/fil-$UID/` with an owner and mode check, the protocol major version in the socket path, and a peer-UID check on every connection. **Process:** starts on the first attach, detached from the login session. **Relay:** `fil server --stdio` writes the preamble and relays stdio to the socket. **Bootstrap:** the version check, key registration, and the reply with the UDP port and fingerprint. **Hosting:** runs `fil-session` sessions and the QUIC listener. On direct attaches it checks `peer_key` against the attach key. Covers `fil ls` and `fil kill`. |
| **P6: the `fil` binary** | new crate `fil` | One static binary for both ends: the CLI (§14), config (`~/.config/fil/config.toml`), and wiring for the client, daemon and both links. Includes `pipe = ["fil", "server", "--stdio"]` for local runs without sshd. |

### Wave 3

| Package | Builds |
|---|---|
| **P7: end to end** | Every scenario below runs on the real `fil`, not stand-ins. **Session events:** attach, kill ssh and resume; a takeover from a second client; a daemon restart giving `SessionReplaced`. **Network:** a NAT rebind mid-session; UDP blocked, falling back to the pipe. **Isolation:** a second Unix user refused at the socket. **Lingering:** `systemd-run` in a container running systemd. **Where:** local runs on macOS through the local pipe command, the realnet suite on GitHub's Linux runners, and the same scenarios on the lab Raspberry Pis. |
| **Checkpoint (owner)** | The owner installs the binary on a Pi by hand and uses it, and tries Tier 3: Wi-Fi to a hotspot, sleep in a bag, real terminals, `just predict-demo`. |

## Carried in from M0 and early M1

| Finding | From | Package |
|---|---|---|
| ssh never notices a silent black hole, so fil heartbeats run over the pipe, plus `ServerAliveInterval` | #13 | P1 |
| QUIC only fails at its idle timeout, so a separate connect timeout (about 2 s) is needed | #13 | P2 |
| An unprompted 4.5 s blackout loses QUIC, so a fresh handshake with the same key follows quickly | #13 | P2 |
| Treat the link as authenticated only after the daemon's first reply; add Retry and rate limiting; close live connections on revoke | #9 | P2, P5 |
| Session incarnation id; resume check order: incarnation, epoch, key; `peer_key` equal to the attach key on direct | #10 | P3, P5 |
| OSC 10/11 replies; snapshot at a sequence boundary; tab stops and charsets in snapshots | #5 | P3 |
| ICANON/ECHO in order with the output, and the input-echo ack | #11 | P3 |
| Answer only `?1049` for the alternate screen (alacritty ignores `?47` and `?1047`) | #5 | P3 (document) |

## Process

1. An author agent builds the package in its own worktree and branch. It runs `just ci`, and `linux-check` when the change is platform-specific, then opens a PR.
2. An independent reviewer agent on Opus reviews the PR from the code, not from the author's summary, and posts findings on the PR.
3. The author fixes the findings on the branch. A changed design goes into DESIGN.md in the same PR.
4. The coordinator does a final check and merges with a squash.

Contract changes (`Link`, the fil-proto wire, `Emulator`) go through the coordinator, because more than one package depends on them.
