# M1 plan: sessions and both transports

> **Historical: the M1 plan.** Every package below is merged (see `git log`) and the sweep is in its last steps. What was built is described in `DESIGN.md` and the other `docs/m1/*.md` files; this file keeps the plan and the order of work.

Owner-approved 2026-11-02. M1 in DESIGN.md §15: daemon and sessions; attach, detach and takeover with epochs; resume and snapshot; the pipe and QUIC with upgrade and fallback. Done when fil is usable as a daily driver for shells.

## Done when

- `fil HOST` attaches through ssh, moves to QUIC within a second where UDP works, and stays on the pipe where it doesn't.
- Wi-Fi changes, laptop sleep, a killed ssh and a daemon restart each end in exactly one of these:
  - a seamless resume, with no gap in native scrollback;
  - a snapshot with a visible gap marker;
  - a clear "taken over" or "session replaced" message.
- Attaching from a second machine takes the session over, and the first one is told.
- The real-network suite (`docs/m1/realnet.md`) passes on the real `fil` binary, not stand-ins. It also runs on the lab Raspberry Pis.
- **An automated drop-in suite** shows fil doing everyday ssh, et and mosh work:
  - interactive tools;
  - long output;
  - reconnects and roaming;
  - takeovers;
  - real hardware.

  It replaces the owner's hands-on checkpoint (owner decision 2026-11-02: no human testing in the pipeline; automate it).
- The M1 sweep is closed (DESIGN.md §15).

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
| **P1: pipe link** | `fil-transport/src/pipe*` | `Link` over any byte stream. **Client side:** it runs the pipe command (`ssh -T -- HOST fil server --stdio` by default, adding `ServerAliveInterval`), scans the preamble and surfaces the skipped text. **Daemon side:** the relay's Unix socket. Frames go through `fil_proto::sched`. Heartbeats and RTT follow §8.5. Liveness: `Degraded` and `Down(Dead)` on time, including the black hole that ssh alone never notices. |
| **P2: QUIC link and path manager** | `fil-transport/src/quic*`, `fil-transport/src/path*` | **The link:** `Link` over quinn, with the §8.5 stream mapping and `auth` from #9. `Up` comes only after the daemon's first frame. It uses `connect_timeout` (about 2 s) and quinn's keep-alive and idle timeout from `LinkParams`. **Daemon side:** stateless Retry and handshake rate limiting, and the connections a revoked key opened get closed. **The path manager** is pure logic: the client's upgrade from pipe to direct, and its fallback and periodic retry (§4.1–4.2). |
| **P3: sessions** | new crate `fil-session` | One session, transport-agnostic: the PTY and child, the output log, and the alacritty emulator. **Detached behaviour:** query replies while detached, including the OSC 10/11 color queries vim sends. **Attaching:** the epoch state machine wired to its `Effect`s, resume or snapshot (snapshots only at escape-sequence boundaries), and the catch-up rule. **Process scope:** `systemd-run --user --scope` where lingering is on, the process tree otherwise. **Messages for M3:** the PTY's ICANON/ECHO mode, reported in order with the output, and an input-echo ack. Tested over `link::mem`. |
| **P4: client** | new crate `fil-client` | The client, independent of the transport. **Terminal:** raw mode and restore, the escapes (`~.`, `~?`, `~~`), and the one-line status overlay while disconnected (on the `fil-predict` overlay core). **Input while disconnected:** buffer, confirm and log (§4.3). **Attaching:** the `TERM` mismatch warning, and the attach, resume and reconnect loop over a dialer that returns links. **Messages:** clear wording for "taken over", "session replaced" and the shell-noise warning. Tested over `link::mem` against a scripted daemon. |

### Wave 1: merged

P1 #16, P3 #18, P2 #19 and P4 #17 landed on 2026-11-02. Each went through the independent review, the author's fixes, one re-check and the coordinator's final check. Each review found real defects; see the PRs.

The wire additions that wave 2 needs are on main too: `SessionExited`, and `Request`/`Response` for `fil ls`/`kill`/`detach` (DESIGN.md §8.5).

### Wave 2 (in parallel)

| Package | Owns | Builds |
|---|---|---|
| **F: wave-1 follow-ups** | the listed items across `fil-transport`, `fil-session` and `fil-client` (no new crates) | **Graceful close:** `Link::flush()`, which waits until every payload sent before the call has gone out (pipe: written to the stream; QUIC: acknowledged by the peer), or fails if the link goes down first. It's implemented for `mem`, pipe and QUIC, and sessions use it before closing after `TakenOver`, `AttachRefused`, `Error` and `SessionExited`, replacing the 300 ms wait. **Exit status:** sessions send `SessionExited`, and the client reports it (`Outcome::SessionEnded { code, signal }`). **The follow-ups listed below.** |
| **P5: daemon** | new crate `fil-daemon` | **Socket and process:** `/tmp/fil-$UID/` with the owner and mode check, the protocol major in the socket path, a peer-UID check on every connection, and starting on first use, detached. **Relay:** `fil server --stdio` writes the preamble and relays stdio to the socket. **Bootstrap:** `Hello`; then `Attach`, which registers the key, replies with the UDP port and fingerprint, and hands the link to `fil-session`; or a `Request` for ls, kill or detach. **Hosting:** `fil-session` sessions behind the `SessionHost` hooks (key revocation through `QuicListener::revoke`, `DirectInfo`), and the QUIC listener, with direct attaches checked as `peer_key` equal to the attach key. Exposes library entry points for the `fil` binary. |
| **P6: the `fil` binary** | new crate `fil` | **Client side:** the CLI (§14), config (`~/.config/fil/config.toml`, including §4.3 offline input and per-host pipe commands), per-attach key generation, and the `Dialer` that drives `fil-transport::path` with `PipeLink` and `QuicLink`, mapping `attached`/`swapped` to the path manager as PR #17 describes. Also `fil ls`/`kill`/`detach` over a `Request`, clear error messages (including a version mismatch from the preamble), and `pipe = ["fil", "server", "--stdio"]` for local runs without sshd. **Server side:** the `fil server` subcommands call `fil-daemon`'s entry points. While P5 is in review they are stubs; they're wired in the same PR once P5 merges, or in a small follow-up. |

### Follow-ups from the wave-1 reviews (package F)

All but the last are done in package F's PR; the last stays an M2 item.

- **Post-takeover race:** check `kicked` right before each output send (#18).
- **Weak tests:** the catch-up-vs-takeover and `kicked` tests must fail without their fixes. `kicked` needs a buffering link, so use `QuicLink` or a buffering test link (#18).
- **Late reaping:** reaping a dropped connection is delayed during a continuous output flood (#18).
- **Gap-marker order:** the gap-marker prologue resets charsets *before* its `?1049l`, and xterm's `?1049l` restores the saved cursor's charset. Leave the alternate screen first, then reset (#17's review).
- **Lost payloads at the stream limit:** on the 1,025th QUIC channel, payloads already accepted with `Ok` are dropped (#19). This is an M2 item. Document it again only if F touches the code.

### Wave 3

| Package | Builds |
|---|---|
| **P7: end to end** | Every scenario below runs on the real `fil`, not stand-ins. **Session events:** attach, kill ssh and resume; a takeover from a second client; a daemon restart giving `SessionReplaced`. **Network:** a NAT rebind mid-session; UDP blocked, falling back to the pipe. **Isolation:** a second Unix user refused at the socket. **Lingering:** `systemd-run` in a container running systemd. **Where:** local runs on macOS through the local pipe command, the realnet suite on GitHub's Linux runners, and the same scenarios on the lab Raspberry Pis. |
| **P8: drop-in suite** | Replaces the owner checkpoint (owner, 2026-11-02). An automated suite uses fil the way people use ssh, et and mosh: vim, less, top, git, shell history and completion, long and fast output, unicode, resizes, background jobs, sleep-like client stalls, reconnects, takeovers and roaming. It runs on the Pis (`pi-e2e`-style) and in realnet. |
| **M1 sweep** | Independent whole-tree audits for redundancy, efficiency, user privacy (DESIGN.md §11.1), security and bugs; fix PRs; a verification pass (DESIGN.md §15). |

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
