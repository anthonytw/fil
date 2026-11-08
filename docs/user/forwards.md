# Forwards and `fil ports`

A port forward belongs to a session, not to a connection: it survives reconnects, a takeover from another machine, and a detach. When the link is down, connections wait and carry on when it returns; the connections already open stay for a while (`linger`, ten minutes by default) after the client goes.

## Kinds

| Flag | Listens | Connects from |
|---|---|---|
| `-L [BIND:]PORT:HOST:HOSTPORT` | here, on your machine | the host |
| `-R [BIND:]PORT:HOST:HOSTPORT` | on the host | here, your machine |
| `-D [BIND:]PORT` | here, a SOCKS proxy | the host |

```sh
fil devbox -L 5173            # localhost:5173 here → port 5173 on devbox
fil devbox -L 8080:db.example.net:5432
fil devbox -R 9000            # port 9000 on devbox → localhost:9000 here
fil devbox -D 1080            # a SOCKS proxy on localhost:1080 that connects from devbox
fil devbox build -L 5173      # the same, for the session "build"
```

The specs are written as in ssh, with these differences:

- `-L PORT` and `-R PORT` mean the same port on the other side's localhost. (ssh's `-R PORT` alone is a SOCKS proxy on the remote; fil has none.)
- **A bare port binds loopback**, on both ends. A `BIND` of `*` or an empty one is every interface, IPv4 and IPv6, and must be written. `0.0.0.0` is every IPv4 interface only, and `[::]` every IPv6 one; `localhost` is loopback on both.
- `-R 0:HOST:HOSTPORT` asks the host for a free port.
- Hosts may be an IPv4 address, a bracketed IP address (`[2001:db8::1]`), or a plain host name. There are no Unix-socket forms, no service names, and `127.1` is refused.
- Repeat a flag for more forwards. A run carries at most 64. Repeating a spec keeps it once, and a later one with the same bind and port but another target replaces it. Two different entries that would listen on the same port are an error.
- A `-L` or `-D` port that can't be bound is reported before your terminal goes raw (a `-R` that the host can't bind is reported later, when the host tries). The `forward_failure` setting in your [config](config.md) decides whether that is only a warning (the default) or ends the run with status 255, like ssh's `ExitOnForwardFailure`.

The list of forwards a host should have can also live in your config; see [`config.toml`](config.md).

## Adding and removing forwards later

```sh
fil forward devbox -L 5173        # add to the session "main" on devbox
fil forward devbox build -R 9000  # to the session "build"
fil forward devbox --rm 5173      # remove the forward that listens on 5173
fil forward devbox --rm -L 5173   # when a -L and a -R share the port, say which
```

Inside a session, leave the host out: `fil forward -L 5173` works on this session, which is `$FIL_SESSION`. The attached client **asks you once** before it listens or connects for a forward added this way, since it may have been asked for by a process on the host. `remote_requests` in your config can answer `deny` instead. Some requests are refused **without asking**: a `-L` or `-D` bound beyond loopback, a `-R` bound beyond loopback, and a `-R` to a target beyond loopback. After three requests in a row are refused (answered no, or not answered), every further one in this run is refused without asking. The first few such refusals are still shown on your terminal.

## `fil ports`

```sh
fil ports devbox            # every session on devbox
fil ports devbox build      # one session
fil ports                   # inside a session: this one
fil ports devbox --json     # machine-readable (schema 1)
```

It shows, per session, each forward with the process behind it, the connections, traffic and last use, and what else is listening, with the one command that would forward it. A sample:

```text
SESSION main on devbox — attached from laptop since 09:12

FORWARDED
  DIR  LAPTOP                REMOTE               OWNER                    CONNS  ↓ / ↑           LAST
  -L   localhost:5173 →      localhost:5173       vite (pid 41872, ~/app)  2      14 MB / 220 KB  now
       http://localhost:5173 · HTTP 200 "Vite + React"
  -R   localhost:9000 ←      127.0.0.1:9000       your laptop              0      —               10:40
       1 connection waiting for the link

LISTENING IN THIS SESSION, NOT FORWARDED
  8000  0.0.0.0 ⚠      python3 (pid 42110, ~/tmp)  since 10:03  HTTP 200 "Directory listing for /"
        ⚠ reachable from outside this host, not just through fil  →  fil forward devbox main -L 8000

AGENT
  relayed to this client: 1 connection open, 14 requests, last used 10:40

OTHER LISTENERS YOU OWN ON THIS HOST
  5432  127.0.0.1  postgres (pid 1201), not started from a fil session  Postgres
```

- A **⚠** marks anything bound beyond loopback, and a forward whose target is held by another user's listener. Another user's listeners are shown by port and address only.
- A forward the host's [server policy](policy.md) refuses says so on its row, with the setting that decided. Refused forwards and a refused agent are listed under `REFUSED BY SERVER POLICY`.
- An incomplete answer says why (a host without `/proc/net/tcp`, a timeout, the 256-row cap).
- **Probes.** To label what is listening (TLS, an HTTP `HEAD`, a Postgres or Redis greeting), fil connects to the listeners that are yours and on loopback: the `-L` targets and the ones started in a session, at most 300 ms per port. A probe is a real connection: a web framework runs its handler for a `HEAD`, a one-shot listener is used up, and Postgres logs a line. `--probe` also probes your other listeners; they are often tunnels (`ssh -L`, `kubectl port-forward`), and a probe through one reaches whatever it leads to, so a known forwarder is never probed. `probe = "off"` in [`daemon.toml`](daemon.md) turns probing off altogether.

`~#`, typed after Enter in a session, shows the forwards and the agent's use on your terminal.

## Who can reach a `-R` port

A `-R` listener on the host is on `127.0.0.1`, which every user on that host can reach, as with `ssh -R`. To narrow it to your own connections, set `remote_peers = "same-user"` in the host's [`daemon.toml`](daemon.md): a connection from another user, or one whose owner can't be told, is reset before a byte reaches your machine. On a macOS host there is no way to look the owner up, so under `same-user` every `-R` connection is refused.

A `-R` bind beyond loopback is honoured only where the host's [server policy](policy.md) allows it (`GatewayPorts`). The host never learns a `-R` target on your own network (it sees `(client network)`), nor a `-L` or `-D` bind that is your machine's own LAN address.
