# `daemon.toml`

The daemon is the program on the host that holds your sessions (`fil server --daemon`, started for you by the first attach). Its settings are in **`~/.config/fil/daemon.toml` on the host where it runs** (`$XDG_CONFIG_HOME/fil/daemon.toml` if that is set). They are the user's own: you write this file for your account, not for other users. For the whole host, see [`policy.toml`](policy.md), which the admin writes and which wins.

- Every key is optional. A missing file means the defaults.
- **Unknown keys are an error,** so a misspelled key doesn't silently do nothing. A file that can't be read or parsed stops the daemon from starting, and the attach reports it.
- The file is read when the daemon starts. A running daemon keeps the settings it started with; to apply a change, end the sessions or wait for the idle daemon to exit (see `idle_exit_secs`), then attach again.
- It can only **narrow** what `policy.toml` and the sshd mirror allow, never loosen it. The policy is checked first, and the policy's keys are not keys of this file.
- The client's own settings are in [`config.toml`](config.md). Keys that belong to the daemon, such as `history_lines` and the port range, are refused there with a message that says to put them here.

## Example

Every key at its default (the four in the last block are commented out because they take the defaults of the client's `[defaults.forward]`):

```toml
# Direct mode (QUIC over UDP). false: no UDP port is opened and every client stays on the pipe.
direct = true
# The address the UDP port binds. The IPv6 wildcard takes IPv4 as well.
bind = "::"
# The UDP ports to try (inclusive), from a random starting point.
port_range = [60000, 61000]
# Lines of scrollback each session keeps for a client that reattaches.
history_lines = 1000
# Seconds with no session and no connection after which the daemon exits. 0: never.
idle_exit_secs = 300
# How sessions group their processes: "auto", "systemd" or "tree".
scope = "auto"
# Give every session a stable SSH_AUTH_SOCK, and allow the agent to be relayed.
agent_socket = true

[ports]
# The protocol probe of `fil ports`: "head", "full" or "off".
probe = "head"
# Also probe the listeners that no fil session started.
probe_other = false

[forward]
# Who may connect to the sessions' -R ports: "any" or "same-user".
remote_peers = "any"
# window = "1MiB"
# buffer = "1MiB"
# open_wait = "30s"
# linger = "10m"
```

## Keys

| Key | Values | What it does |
|---|---|---|
| `direct` | `true` (default), `false` | `false` opens no UDP port, so direct mode is off for every client of this daemon. |
| `bind` | an IP address; default `::` | The address the UDP port is bound to. `::` also takes IPv4; on a host without IPv6 fil falls back to `0.0.0.0`. Set a LAN or tailnet address to keep the port off other interfaces. |
| `port_range` | `[first, last]`; default `[60000, 61000]` | The UDP ports to try, inclusive, from a random starting point. `first` must be at least 1 and not above `last`. Open this range in the host's firewall; see [transports](transports.md). |
| `history_lines` | 1 to 1,000,000; default 1000 | Lines of scrollback each session keeps for a client that reattaches. Memory only. |
| `idle_exit_secs` | seconds; default 300; `0` never | The daemon exits on its own after this long with no session and no connection. |
| `scope` | `"auto"` (default), `"systemd"`, `"tree"` | How a session's processes are grouped, so that `fil kill` and `fil ports` can find them all. `auto` uses a systemd user scope where lingering is on and the process tree otherwise; `systemd` always, and a session fails to start if it can't; `tree` always the process tree. |
| `agent_socket` | `true` (default), `false` | `false`: sessions get no `SSH_AUTH_SOCK` and [the agent](agent.md) is never relayed. |

### `[ports]`

| Key | Values | What it does |
|---|---|---|
| `probe` | `"head"` (default), `"full"`, `"off"` | The protocol probe of [`fil ports`](forwards.md), on your own loopback listeners only: `head` does a TLS handshake, an HTTP `HEAD` and the Postgres and Redis greetings; `full` does an HTTP `GET` as well, for the page title; `off` probes nothing, and `fil ports --probe` too. A probe is a real connection. |
| `probe_other` | `false` (default), `true` | Probe your other listeners as well as the `-L` targets and the ones a session started. They are often tunnels, and a probe through one reaches whatever it leads to. A listener a known forwarder holds is never probed. |

### `[forward]`

| Key | Values | What it does |
|---|---|---|
| `remote_peers` | `"any"` (default), `"same-user"` | Who may connect to the sessions' `-R` ports. `any` is every local user, as with `ssh -R`. `same-user` resets a connection from another user, or from one whose owner can't be told, before a byte reaches the client, and `fil ports` counts those. On macOS there is no way to tell, so `same-user` refuses every `-R` connection there, and the daemon logs a warning. |
| `window` | `"16KiB"` to `"4MiB"`; default `"1MiB"` | The receive window per channel. |
| `buffer` | `"16KiB"` to `"16MiB"`; default `"1MiB"` | What a channel's sender keeps unacknowledged. |
| `open_wait` | `0` to `"1h"`; default `"30s"` | How long a `-R` connection waits for a link. |
| `linger` | `0` to `"24h"`; default `"10m"` | How long open connections outlive a disconnect, and how long a `-R` entry whose target the host never learned outlives its run (at least 30 seconds). |

Sizes are numbers of bytes or text like `"256KiB"`; durations are seconds or text like `"30s"`, `"1500ms"`, `"10m"`. These four are the daemon's side of the client's `[defaults.forward]` tunables, with the same bounds.
