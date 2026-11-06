# Quick start

This page assumes fil is [installed](install.md) on your machine and that `ssh devbox` already works. Everything ssh does for you, including `~/.ssh/config`, `ProxyJump`, certificates and your agent, applies unchanged: fil starts the system `ssh` to log in.

## Attach

```sh
fil devbox
```

This logs in with ssh, starts fil's daemon on `devbox` if it isn't running (as you, no root), and attaches to a session called `main`, creating it. You get your login shell. Your terminal's own scrollback works as it always does: fil does not redraw the screen from a model while you are connected.

If `devbox` has no fil yet, fil [offers to install it](install.md).

## Lose the connection

Close the lid, change Wi-Fi, pull the cable. When the network comes back, fil reconnects by itself and carries on where the stream stopped; the screen and the scrollback are exactly as they were. While it is down, a short status overlay says what fil is doing. Keys you type meanwhile are kept in memory (4 KiB by default) and sent when the link returns; see [transports](transports.md).

## Detach and come back

Type `~.` right after pressing Enter, as in ssh. You get your terminal back at once. The session, its shell and everything it started keep running on `devbox`.

```sh
fil devbox            # attach to "main" again
fil ls devbox         # the sessions on devbox
```

A second machine can do the same: `fil devbox` from your desktop takes the session over from your laptop. See [sessions](sessions.md).

## More than one session

A session is a name, `[A-Za-z0-9._-]`, up to 64 characters:

```sh
fil devbox build      # a session called "build"
fil kill devbox build # end it: its shell and everything it started
```

## Port forwards

```sh
fil devbox -L 5173            # localhost:5173 here reaches port 5173 on devbox
fil devbox -R 9000            # port 9000 on devbox reaches localhost:9000 here
fil ports devbox              # what each session holds, and what is listening
```

The forwards belong to the session and survive reconnects. See [forwards](forwards.md).

## Where next

- [Sessions, detach and takeover](sessions.md)
- [Reconnects and transports](transports.md), if UDP or a firewall is in the way
- [`config.toml`](config.md), to set things per host
- [Troubleshooting](troubleshooting.md)
