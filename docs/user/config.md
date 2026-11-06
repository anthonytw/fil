# The client configuration: `config.toml`

fil reads `~/.config/fil/config.toml` (or `$XDG_CONFIG_HOME/fil/config.toml`) when it starts. A missing file means the defaults. `--config FILE` or `$FIL_CONFIG` names another file; a file named that way must exist.

Every key is optional. An unknown key is an error, so a typo cannot silently do nothing; the message names the file, the section and the key. Every section is checked when the file is read, whichever host you ask for.

A complete example is in [examples/config.toml](examples/config.toml). The host's own settings are in `daemon.toml` ([examples/daemon.toml](examples/daemon.toml)) and, for the host's administrator, `/etc/fil/policy.toml` ([examples/policy.toml](examples/policy.toml)); this page covers the client's file only.

## How a host's settings are chosen

```toml
[defaults]                # applies to every host
transport = "auto"

[host.devbox]             # selected by the HOST you type: `fil devbox`
transport = "pipe"
```

- `[host.NAME]` overrides `[defaults]` key by key. The nested tables (`forward`, `offline_input`, `direct`) are merged key by key too. A list replaces the list under it.
- `fil user@devbox` uses `[host."user@devbox"]` if there is one, and otherwise `[host.devbox]`.
- The command line is applied over both: `-L`, `-R` and `-D` add to `forward`, `-A` and `-a` set `agent`, `--transport` sets `transport`.

## Seeing what applies: `fil config show HOST`

```
fil config show devbox
```

prints the effective value of each key for `devbox` and where it came from: `default` (nothing in the file sets it), `[defaults]` or `[host.devbox]`. It reads only the config file. It does not contact the host, start ssh or touch the daemon, so it is safe to run anywhere, as `ssh -G` is. The command line options are not in its answer.

## Keys

Keys go in `[defaults]` or in a `[host.NAME]` section.

### `transport`

`"auto"` (default), `"pipe"` or `"direct"`. `auto` starts on the ssh pipe, dials direct (UDP) in the background and moves to it when it is up, falling back to the pipe if direct is lost. `pipe` never dials. `direct` is like `auto`, but waits longer before it runs ssh again after losing direct. The first attach always goes through the pipe.

### `pipe`

A list: the command that carries the session instead of `ssh -T -- HOST fil server --stdio`. It is run directly, never through a shell. Use it for anything that is not plain ssh (`kubectl exec`, a container, a bastion command). With a custom pipe, set [`direct.address`](#directaddress) if you want direct mode.

### `history_lines`

Refused here with a message that points to `daemon.toml`. The scrollback kept per session is a setting of the daemon on the host, not of the client.

### `host_label`

The name other clients are shown when you take a session over, 1 to 128 bytes of text. Default: this machine's short host name.

### `remote`

`"auto"` (default), `"plain"` or `"installed"`: which command runs on the host. `auto` runs `fil server --stdio` first and uses the install bootstrap if the host has no fil. `plain` never uses the bootstrap. `installed` uses the bootstrap from the start.

### `install`

`"ask"` (default), `"never"` or `"always"`: whether an attach that finds no fil on the host offers to install it. It asks only on an interactive first attach.

### `forward`

Forwards for every attach to the host: a list of the same specs the flags take.

```toml
forward = ["-L 5173", "-R 9000", "-D 1080"]
```

A bare port binds loopback only. Written as a table, `forward` also holds the tunables below; the list is then its `specs` key.

### `forward.specs`

The list of specs, when `forward` is a table because tunables are set too.

### `forward.window`

The receive window each forwarded connection advertises, `"16KiB"` to `"4MiB"`. Default `"1MiB"`.

### `forward.buffer`

What a forwarded connection's sender keeps until the other side has consumed it, `"16KiB"` to `"16MiB"`. Default `"1MiB"`.

### `forward.open_wait`

How long a new connection waits for a link while the session is disconnected, `0` to `"1h"`. Default `"30s"`.

### `forward.linger`

How long open forwarded connections outlive a disconnect, `0` to `"24h"`. Default `"10m"`. The host keeps a withheld entry for the `linger` of its own `daemon.toml`, never for this one.

### `agent`

`"off"` (default), `"on"` or `"confirm"`: relay the local ssh-agent to the session. `confirm` asks on screen before each signature. `-A` and `-a` override it for one run.

### `agent_keys`

A list of SHA256 fingerprints (`SHA256:...`, as `ssh-keygen -lf KEY.pub` prints them), at most 64. The relay answers only for these keys. Empty (default) means every key the agent holds.

### `remote_requests`

`"ask"` (default) or `"deny"`: what the client does when a process on the host asks for a forward with `fil forward`. `ask` asks you once per entry.

### `forward_failure`

`"warn"` (default) or `"exit"`: what a forward that cannot be set up does. `warn` says so and carries on; `exit` ends the run with status 255, as ssh's `ExitOnForwardFailure`.

### `offline_input`

A table for keystrokes typed while disconnected. Its keys are below.

### `offline_input.buffer`

How much typing is held while the link is down, as a size (`"4KiB"`) or a number of bytes. `0` drops it. Default `"4KiB"`.

### `offline_input.confirm`

`"off"` (default) or `"after 10s"` (any whole number of seconds): after a disconnect that long, show what was typed and ask before sending it.

### `offline_input.log`

Keeps everything typed while disconnected. Off by default, because it can capture passwords. A path (absolute, or starting with `~/`) is appended to with mode 0600; a list is a command that receives the bytes on its standard input, never through a shell.

```toml
log = "~/.local/state/fil/offline-input.log"
log = ["logger", "-t", "fil"]
```

### `direct`

A table for the direct (UDP) transport. Its keys are below.

### `direct.address`

Where to dial UDP, when it is not the name you typed (an ssh alias, a different interface). A host name or an IP address with no port: the daemon announces the port. Needed with a custom `pipe` or a jump host.

### `direct.port_range`

Refused here with a message that points to `daemon.toml`. The UDP port range is chosen by the daemon on the host.

## Tables

### `defaults`

Settings for every host.

### `host`

Settings for one host: `[host.NAME]`, where NAME is the HOST argument as typed.

## No `predict` key in 0.1.0

0.1.0 has no predictive echo, so there is no `predict` key. A file that has one is refused as an unknown key. The key comes back with prediction in 0.2.0.
