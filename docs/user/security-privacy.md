# Security and privacy

This page says in plain words what fil trusts, what it exposes, and what it records about you. The rules behind it are in the project's `DESIGN.md` (sections 11 and 11.1); every release is checked against them.

## What fil trusts

**SSH is the root of trust.** fil adds no accounts, no key registry and no identity system of its own. An attach starts with an ordinary ssh login as you; everything ssh does to authenticate applies.

Four rules hold for every fil feature:

1. It needs no root on the host, no system service and nothing setuid.
2. It keeps no secrets at rest. The key that authenticates a direct connection is made in your client's memory for one attach, is bound to the one session it was made for, and is revoked when you detach, when someone takes the session over, and when the session ends. Crash recovery deliberately goes through a fresh login. **The one exception you can turn on** is the offline-input log (below).
3. Its attack surface is small. Pipe mode opens no port. Direct mode adds exactly one authenticated UDP port per user.
4. The commands fil runs on the host are fixed. Over ssh they are `fil server --stdio`, and one constant install command that holds only the build's version and wire; neither is ever built from your input, because ssh passes its command to the remote shell as a string. Session names travel inside the protocol and are validated at both ends.

## Who could reach what

| Who | What they could reach | Protection |
|---|---|---|
| Anyone on the network | sshd; the UDP port, when direct mode is on | OpenSSH as before. The QUIC handshake succeeds only with the per-attach client key registered over the ssh login. Everything else is dropped without an answer; floods are rate-limited. |
| A man in the middle | the direct connection | The client pins the daemon's certificate fingerprint, received over the ssh login. Nothing is trusted on first use. |
| Someone who steals a client key | the session | The key exists only in the memory of the client, for one attach, and is revoked on detach and takeover. |
| Other users on the host | the daemon's socket | `/tmp/fil-$UID/` is yours and mode 0700; the daemon refuses a directory made by someone else, and checks every connection's user. |
| Other users on the host | your `-R` ports on `127.0.0.1` | Reachable by every user there, as with `ssh -R`. `remote_peers = "same-user"` in [`daemon.toml`](daemon.md) narrows it. |
| The host using your agent | signatures | Off by default; see [the agent](agent.md). |
| The host's admin | what you forward | fil respects their sshd settings and `policy.toml`; see [for host admins](policy.md). A courtesy, not a boundary. |
| A laptop waking up later | your session | A client holding an old copy of the session is refused and asked before it takes the session back. It never steals it silently. |
| A stolen laptop disk | the offline-input log, if you turned it on | No tokens or keys are stored on disk. |

**Out of scope:** root on the host, a compromised client machine, and the terminal emulator's own escape-sequence handling. fil passes bytes through exactly as ssh does.

## What fil reveals about you (privacy)

1. **Nothing leaves your machines.** fil talks only to the hosts you point it at, through the pipe command you chose. There is no telemetry, no update check, no crash reporting and no third-party service.
2. **Logs record what fil does, not what you do.** No log at any level has terminal output, keystrokes, command lines, environment variables or offline input. Identifying details (host names, addresses, user names, session names, paths) are kept to a minimum: session names and host labels are logged at `debug` only. Error lines can still carry a path or other detail from the operating system's message, for example when a file can't be opened. Logs are private to you (files 0600, directories 0700), rotated at 4 MiB or a week, and deleted a week after their last write, so nothing older than about two weeks stays.
3. **Nothing is kept that you didn't ask for.** A session's output, history and snapshots are in the daemon's memory only. The one thing that can keep what you typed is the **offline-input log** (a file, or a command you name that receives the bytes), which is off by default because it can capture passwords. When you turn it on it is mode 0600, rotated at 1 MiB and at 7 days, deleted a week after its last write, and refused if its directory is writable by others. It is tidied at the start of every run even when the log is off.
4. **Others see as little as possible.**
   - On the network, the TLS name is a fixed `fil.invalid`, and the daemon's certificate carries no host or user name.
   - Your other clients see the host label of the machine that attached, which you can set. It is never sent to anyone else.
   - The host never learns a `-R` target on your network, nor a `-L` or `-D` bind that is your machine's LAN address.
   - The remote session's environment drops what describes the login that happened to start the daemon: `SSH_CONNECTION`, `SSH_CLIENT`, `SSH_TTY`, the login's agent, X11 and Kerberos variables, `XDG_SESSION_*` and the like. The stable `SSH_AUTH_SOCK` path does contain the session's name, as `FIL_SESSION` already does; a tool that uploads its environment could carry it off the host.
   - Session names stay out of systemd unit names and descriptions, which other local users and the journal can read.
5. **No surprises on the host.** fil creates nothing there beyond its state directory (`~/.local/state/fil`), its socket directory under `/tmp`, and, if you install through fil, `~/.local/share/fil/<version>-w<wire>/` (0700). It removes what it no longer needs.

### Where fil writes on your machine

- `~/.local/state/fil/client.log`, the client's log. `FIL_LOG=debug` adds detail, including the identifying details above.
- The offline-input log, if you set `offline_input.log`.

Your config is `~/.config/fil/config.toml` and is yours; fil only reads it.
