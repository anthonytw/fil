# Reconnects and transports

fil can carry a session over two transports and moves between them by itself.

| Transport | What it is | Needs |
|---|---|---|
| **pipe** | the output of `ssh -T -- HOST fil server --stdio`: your ssh login is the connection | only what ssh needs |
| **direct** | QUIC over UDP, straight to the daemon on the host | a UDP port on the host that your client can reach |

## How an attach goes

1. fil runs ssh and attaches over the pipe. The session starts at once, with no extra round trips. This is the only ssh login of an attach, so Touch ID, passwords and host-key questions work as in ssh.
2. The host's reply carries a UDP port and the fingerprint of the daemon's certificate. In the background, your client dials QUIC. If that works, it moves the session to QUIC and closes the pipe.
3. If UDP is blocked, the session stays on the pipe, and fil tries QUIC again from time to time and right after a network change.

## Reconnecting

- Both transports send a heartbeat every second. A link that is silent for five seconds is declared dead.
- **On QUIC,** a change of your IP address (Wi-Fi to a phone, a new network) is absorbed without a new handshake. If the connection is gone altogether, for example after the laptop slept, the client makes a new QUIC handshake with its key from memory. **No ssh is involved.**
- **If QUIC can't be re-established** within a few seconds, fil runs the pipe again, which is a new ssh login. In the background that login can't ask you anything: if it needs a password, passphrase or host-key answer, the status line says “ssh needs an interactive login: detach (~.) and run fil again”, and fil keeps retrying so an agent you unlock elsewhere recovers it.
- Retries back off, with jitter. A link that dies right after coming up counts as a failure, so a flapping network slows fil down instead of making it cycle.
- Whichever transport carries it, the client says "resume this session from byte N" and the daemon sends everything after it.

While the link is down, the client draws a short inverse-video status near the edge of the screen farthest from the cursor, and removes it when the link returns, leaving the screen and scrollback as they were.

### Typing while disconnected

Keystrokes you type while disconnected are held in memory, up to a cap (`offline_input.buffer`, 4 KiB by default; `0` drops them), and sent when the link returns. The status overlay says how much is held and how much was dropped. With `offline_input.confirm = "after 10s"`, a long outage makes fil show what you typed and ask before sending, so a Ctrl-C typed in a long outage can't land late by surprise. `offline_input.log` keeps everything typed while disconnected in a file or a command; it is **off by default** because it can capture passwords. See [`config.toml`](config.md).

## Choosing a transport

`transport` in your [config](config.md), or `--transport` for one attach:

| Value | Meaning |
|---|---|
| `auto` (default) | the pipe first, then QUIC |
| `pipe` | ssh only; fil never dials UDP |
| `direct` | as `auto`, and after QUIC is lost an ssh re-login waits 10 seconds for QUIC to come back |

```sh
fil devbox --transport pipe
```

## UDP and firewalls

Direct mode uses **one UDP port per user on the host**, not one per session. The daemon picks it from `60000` to `61000` inclusive, starting at a random point; that is the range mosh uses, so a host already open for mosh works unchanged. In `daemon.toml` you can change the range (`port_range`) and the address it binds (`bind`); see [`daemon.toml`](daemon.md).

- **Open UDP 60000-61000 to the host** (or the range you set) for direct mode. Without it, fil stays on the pipe and works, with ssh's behavior on a bad network instead of QUIC's.
- **The client dials the name or address ssh would use:** for the default pipe, the host name that `ssh -G HOST` reports, so an alias from `~/.ssh/config` works. For a host behind `ProxyJump` or `ProxyCommand`, and for any custom `pipe`, fil does **not** dial unless you set `direct.address` for that host: the name belongs to the jump host, and a lookup would tell your network's resolver where you are going.
- **No UDP at all on the host:** an admin can switch direct mode off (`direct = false` in [`policy.toml`](policy.md), or `direct = false` in your own `daemon.toml`). Then no UDP port is opened and every client stays on the pipe.
- **Nothing is trusted on first use.** The daemon's certificate fingerprint arrives over the ssh login and is pinned; the client authenticates with a key made in memory for this attach and registered over the same login. Anything else that reaches the port is dropped. See [security and privacy](security-privacy.md).

## A custom pipe

`pipe` in your config is a command whose stdin and stdout reach `fil server --stdio` on the host. The default is `ssh -T -- HOST fil server --stdio`; it can be `kubectl exec -i`, `docker exec -i`, `tailscale ssh` and the like. A custom pipe runs as configured. It is not dialed over UDP unless you set `direct.address`, and fil never installs through it.
