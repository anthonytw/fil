# For host admins: `/etc/fil/policy.toml` and the sshd mirror

This page is for the person who runs a host that users connect to with fil. You may never have heard of fil before. In short: fil is a program your users copy into their home directories. It needs no root, no service and no open port beyond the ones it asks for. It respects what **you** decided about forwarding, in two layers, and refuses when it can't tell.

**This is a courtesy, not a security boundary.** As `sshd_config(5)` says of its own forwarding switches, a user who has a shell can run any program they like, including their own relay. fil's policy keeps well-behaved tools in line with what you meant; it does not stop a determined user.

## What fil does on your host

- Each user who runs it gets one daemon, as themselves, started by their first login. There is no system-wide service and nothing setuid.
- The daemon holds the user's sessions (a shell each), in memory. Its socket is in `/tmp/fil-$UID/` (mode 0700), and every connection's peer user is checked.
- It uses ssh for authentication. An attach is an ordinary ssh login that runs `fil server --stdio`. fil adds no accounts and no keys of its own.
- **Direct mode** adds one UDP port per user, in 60000-61000 by default (the range mosh uses). The port accepts only a client holding a key registered over the user's ssh login.
- A user may install fil into `~/.local/share/fil` through their own ssh login. Nothing is installed anywhere else.

## Layer 1: the sshd mirror

fil reads `/etc/ssh/sshd_config` and the files it includes, and applies the forwarding settings **as sshd would for the daemon's user**.

| sshd setting | What it governs in fil |
|---|---|
| `DisableForwarding` | all forwards and the agent |
| `AllowTcpForwarding` (`yes`/`all`, `no`, `local`, `remote`) | `-L` and `-D`; `-R` |
| `AllowAgentForwarding` | the agent relay |
| `PermitOpen` | the targets of `-L`, and of each `-D` SOCKS request |
| `PermitListen` | the binds of `-R` |
| `GatewayPorts` | `-R` binds beyond loopback: only with `yes` or `clientspecified` |

Every other keyword is skipped. How the mirror reads the file:

- **Parsing is sshd's:** keywords are case-insensitive, `keyword value` or `keyword=value`, `#` starts a comment, the first value of a setting wins, and the defaults are OpenSSH's (forwarding and the agent allowed, `PermitOpen` and `PermitListen` `any`, `GatewayPorts no`).
- **`Match`:** `User` and `Group` (patterns with `*`, `?` and a leading `!` in lists) are evaluated against the daemon's user. Any other criterion (`Address`, `Host`, `LocalAddress`, `LocalPort`, `RDomain`, `all`, and so on) **might apply**, and then the stricter of the block's value and the outer one wins.
- **`Include`** takes globs relative to `/etc/ssh`, in sorted order, at most 16 deep. A file included from a `Match` block is inside it.
- **Strict when unsure.** A value fil doesn't know is read as the strict one. A line that fil can't evaluate the way sshd would (for example an `Include` glob that differs between systems, or a file with a NUL byte) makes the settings **unknown**: every forward and the agent are refused, naming the file and line, unless `policy.toml` sets the key itself.
- **Unreadable files.** A main `sshd_config` that fil can't read (RHEL and Fedora keep it mode 0600) is not an error: fil uses OpenSSH's defaults, so a wide `-R` is still refused. An included file or directory it can't read is skipped, the rest is still enforced, and `fil ports` says that the mirror is incomplete, naming the file or directory but nothing in it.

The mirror reads the file as the daemon's user. If your users cannot read `sshd_config`, the defaults stand in; put the settings you want in `policy.toml`.

## Layer 2: `/etc/fil/policy.toml`

A file that **you** write. Its keys override what is mirrored, to tighten or explicitly to loosen. A key left out keeps the mirror.

```toml
# Each key overrides what fil mirrors from sshd_config; a key left out keeps the mirror.
local = true              # -L and -D (instead of AllowTcpForwarding and DisableForwarding)
remote = true             # -R
agent = true              # the agent relay (instead of AllowAgentForwarding and DisableForwarding)
direct = true             # fil's direct (QUIC) transport; false: no UDP port, attaches use the pipe
permit_open = ["any"]     # -L and -D targets, as PermitOpen: "any", "none", or "host:port" ("*" for either)
permit_listen = ["any"]   # -R binds, as PermitListen: "any", "none", "port" or "host:port"
gateway_ports = false     # -R beyond loopback (as GatewayPorts clientspecified when true)
```

| Key | Values | Meaning |
|---|---|---|
| `local` | `true`, `false` | allow or refuse `-L` and `-D`, instead of `AllowTcpForwarding` and `DisableForwarding` |
| `remote` | `true`, `false` | allow or refuse `-R` |
| `agent` | `true`, `false` | allow or refuse the agent relay, instead of `AllowAgentForwarding` and `DisableForwarding` |
| `direct` | `true`, `false` | `false` refuses fil's direct transport, which sshd has no switch for: the daemon **opens no UDP port**, offers no direct mode, and drops a QUIC resume, so clients use the pipe |
| `permit_open` | a list of `"any"`, `"none"`, or `"host:port"` / `"[v6]:port"` entries; `*` stands for either part | as `PermitOpen`: the targets of `-L` and of each `-D` request |
| `permit_listen` | a list of `"any"`, `"none"`, `"port"`, or `"host:port"` entries; `*` stands for either part | as `PermitListen`: the binds of `-R` |
| `gateway_ports` | `true`, `false` | `-R` binds beyond loopback; `true` is `GatewayPorts clientspecified`, `false` is `no` |

`any` and `none` must be the whole list. An empty list, or an entry fil can't read (a port of 0, say), makes the file unusable, and then everything is refused (below).

Because the file stands in for the mirror's settings key by key, a policy file that sets all of `local`, `remote`, `agent`, `permit_open`, `permit_listen` and `gateway_ports` leaves nothing for the mirror to decide, which also rescues a host whose `sshd_config` fil can't evaluate.

### When the file is trusted, and when everything is refused

fil uses the file only if it is:

- a regular file, not a symbolic link;
- owned by root;
- not group- or world-writable;
- in directories that are owned by root and not group- or world-writable, all the way up. A symbolic link on the way must be root's, and its target directories must pass too.

**Absent is fine:** the sshd mirror alone decides.

**Present but failing a check, or not parsing, refuses every forward and the agent,** naming the file. An unknown key, a duplicate key, a value of the wrong type, an empty list and invalid UTF-8 all count as not parsing, and so does a file the daemon's user can't read. This is deliberate: someone who wrote a policy file meant something by it, and a file fil can't trust or read must not open what they meant to close.

```sh
sudo install -m 0644 -o root -g root policy.toml /etc/fil/policy.toml
```

## Where it is enforced

On the host, in the daemon, every time a session would let a forward or the agent exist:

- when a forward is **added**: from the attached client's command line or config, from `fil forward`, or by takeover;
- when the forward table is **re-established** for a client, at every fresh attach and resume: a `-R` listener the policy now refuses is closed, one it allows again is bound again;
- on **every connection** through a forward, including each SOCKS target of a `-D`, and each accepted `-R` connection;
- when the **agent socket** would be created, and on every agent connection.

A user's own `daemon.toml` can only narrow the policy, never loosen it.

## Changes take effect without a restart

At most once a second, a check looks at the metadata of every file the policy was read from (and each directory a glob listed, and the one the policy file sits in), and re-reads everything when any of it changed, and every five minutes in any case. The reading is done in the background, never in the check, so a slow directory service holds up no session. A read that isn't known to be current for too long decides nothing, and checks refuse until the re-read is in.

There is no signal for it: an admin who edits `sshd_config` reloads sshd, not fil daemons they may not know are there, so fil would never get the signal. A tightened policy reaches the running sessions of your users within a few minutes: forwards it now refuses are closed.

## What users see

A refusal names the deciding setting and its source, for example:

```text
REFUSED BY SERVER POLICY
  -R *:8080:localhost:80: GatewayPorts no (OpenSSH's default)
  agent forwarding: AllowAgentForwarding no (/etc/ssh/sshd_config:12)
```

Only the deciding setting leaves the host, not the rest of your configuration. `fil ports` also says, in a `SERVER POLICY` block, when `sshd_config` or an included file can't be read, when a line can't be evaluated, when `policy.toml` refuses everything, and when direct mode is refused.

## Privacy

Nothing of the config is logged above debug level, and the debug lines say what happened, not what your files hold. See [security and privacy](security-privacy.md).
