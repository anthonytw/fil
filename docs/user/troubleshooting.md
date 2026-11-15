# Troubleshooting

## First look

```sh
fil --version                  # fil <version> (wire <major>.<revision>)
fil install --check devbox     # what fil is installed on devbox, and the fil on its PATH
ssh devbox fil --version       # what a fil login there finds
```

`fil install --check HOST` changes nothing. It lists the copies fil installed under `~/.local/share/fil`, the `fil` on the `PATH` and `~/.local/bin/fil`, and prints them. If the host's report is cut short, or a forced command prints nothing and exits 0, `--check` fails with "no answer from the host" rather than claiming nothing is installed.

## Logs

- Client: `~/.local/state/fil/client.log`
- Host: `~/.local/state/fil/daemon.log` on the host

Both are JSON lines, private to you, and rotated. They are quiet by default. `FIL_LOG` sets the level for fil's own crates (`FIL_LOG=debug fil devbox`), and `FIL_LOG=fil=debug,quinn=trace` names other crates. A name that isn't a fil crate is silent unless named, because the QUIC library at `trace` logs every packet. Session names and host labels are in the logs at `debug` only.

## "fil is not installed on the remote host"

Neither the `PATH` of a non-interactive ssh login there nor fil's install directory (`~/.local/share/fil`, and `~/.local/bin`) has a `fil` of this wire. That `PATH` is often shorter than the one you see when you log in; `ssh devbox 'echo $PATH'` shows the one that counts. Either let fil [install itself](install.md) (`fil install devbox`), or copy a release's `bin/fil` into a directory on that `PATH`, or into `~/.local/bin`, as a file, not a symbolic link (`~/.local/share/fil` is for the copies fil installs itself).

With `remote = "plain"` in your [config](config.md), only `fil server --stdio` runs, fil never tries to install, and the message names that command. fil explains a missing `fil` in its own words: the remote shell's own "command not found" line is not shown.

## "Wire version mismatch", or "the fil on the remote host is older"

Two fil builds of `wire 1.16` or later (in `fil --version`) always talk to each other. Older builds talk only when their wire numbers are equal, and builds of different majors (`wire 1.x` and `wire 2.x`) never do. The message says which side is newer and which to upgrade. By default fil runs its install command first, which never starts a fil of another wire: it starts a copy of the right version if the host has one, and otherwise offers to install it. You see this message with `remote = "plain"`, behind a forced command, or when the copy it started is of another wire. Then upgrade the older side: [install](install.md) the same version on the host.

Daemons of different wires coexist on a host, so an upgrade breaks no running session. Sessions of a daemon of an older, incompatible wire (`wire 1.15` and before) keep running there; a `wire 1.16` or later fil does not list them, and the old fil still reaches them.

## A patch release, and daemons that are still running

Each version runs its own daemon, so the new version's first connection starts a daemon of its own beside the one already running, new sessions go to it, and the old daemon exits as soon as its last session ends. From `wire 1.17` on, `fil ls` lists the old daemon's sessions too (its FIL column names the version), and `fil HOST SESSION` and `fil kill` reach them there. If an old session does not show up or is not reached, the fil that runs on the host is older than `wire 1.17` (with `remote = "plain"`, it is the one installed there): `fil ls` then names the other daemon in an "also running" line with the reason, and upgrading the host's fil fixes it. To end every session of the older daemons at once, `fil kill --outdated HOST`. While several daemons run, `fil ls` and `fil kill --outdated` make an ssh connection per daemon (and `kill --outdated` more: [sessions](sessions.md#after-an-upgrade-daemons-of-several-versions)); if each one asks for your password, set `ControlMaster auto` and `ControlPersist` for the host in `~/.ssh/config`. An attach to a session while several daemons run asks each daemon whether it holds the session first, all at once and for at most two seconds, so a daemon that hangs delays an attach by up to two seconds, never more. (Before 1.0 a patch release reused the running daemon, and its fixes waited until that daemon exited.)

## "This host kills a user's processes when the last login ends"

When fil starts a daemon on a host whose `logind.conf` says `KillUserProcesses=yes` and where you do not linger, it prints:

```text
this host kills a user's processes when the last login ends (KillUserProcesses=yes in logind.conf) and lingering is off, so the fil daemon and every session will end at logout; run `loginctl enable-linger` once to keep them
```

Your sessions will end when your last login does, which is the opposite of what you want from fil. Run `loginctl enable-linger` once on that host (it may need an admin), and the daemon and sessions stay. Only an explicit `yes` warns.

## Your home directory is a symbolic link

`fil install` refuses a `$HOME` that is itself a symbolic link, and the install directory's parents must be real directories owned by you (or root) that nobody else can write. Install by hand instead: copy `bin/fil` to a directory on the non-interactive `PATH`. See [install](install.md).

## Hosts that allow only the plain command

Some hosts accept only `fil server --stdio`: a `ForceCommand` in `sshd_config`, or `command=` in `authorized_keys`. The install command can't run there, and fil can't install itself; install fil on it by hand, on the `PATH` the gate uses. With the default `remote = "auto"`, fil sees the gate answer its install command and runs `fil server --stdio` instead, which costs a second ssh login on each run; set `remote = "plain"` for that host in your [config](config.md) to save it. With `remote = "installed"` on such a host, fil ends the run at once and tells you to install fil there by hand or use `plain`.

A host whose config names its own `pipe` is never rewritten and is never offered an install.

With the default `remote = "auto"`, fil's install command needs `sh`, `id`, `ls`, `grep` and `uname` on the host, and finds `fil` the way `sh` does: a `fil` that is only a shell function or alias in your zsh or fish setup is not found. On such a host set `remote = "plain"`, which runs only `fil server --stdio` through your login shell.

## A home directory mounted `noexec`

The upload arrives and passes its checks, but the script can't run it. fil says the home directory may be read-only or mounted `noexec`; the leftover `.new-*` directory is removed by cleanup after 24 hours. Copy fil to a directory that allows execution and put it on the `PATH`.

## ssh needs an interactive login

Once a session is attached, fil's reconnects run ssh in the background, where it can't ask you anything. If that login needs a password, a passphrase or a host-key answer, the status line says “ssh needs an interactive login: detach (~.) and run fil again”, and shows ssh's own last line. fil keeps retrying, so unlocking your agent elsewhere recovers it. A key in an agent, a certificate or `ControlMaster` avoids it.

## It stays on the pipe: direct mode never starts

The session works but `transport = "direct"` or `auto` never leaves ssh:

- UDP 60000-61000 (or your `port_range`) is blocked between you and the host. Open it, or accept the pipe.
- The host is behind `ProxyJump` or `ProxyCommand`, or you use a custom `pipe`. Set `direct.address` for that host in your [config](config.md).
- The host's `policy.toml` or your `daemon.toml` has `direct = false`.

## A forward is refused

`fil ports` names the setting: `REFUSED BY SERVER POLICY` is the host's sshd or `policy.toml` (see [for host admins](policy.md)); a row marked "not listening" is a port in use on your machine. A `-R` beyond loopback needs the host to allow `GatewayPorts`.

## Exit codes

`0` normal, `1` error, the session was taken over elsewhere or you did not take it over, `2` usage error, `255` a forward failed under `forward_failure = "exit"`, and `128+N` for a signal (`fil -N` exits `0` on a stop signal: that is how it is stopped; `255` if the signal came before it was attached). See [sessions](sessions.md).

## Getting help

`fil -h` or `fil --help`, `fil COMMAND --help`, `fil -V` or `fil --version`, and the manual pages `fil(1)`, `fil-config(5)` and `fil-policy(5)`.
