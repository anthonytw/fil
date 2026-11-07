# The agent

fil can relay your local ssh-agent to a session, so that `git push` or `ssh` on the host can use your keys. It is **off by default**.

## Turning it on

```sh
fil devbox -A         # relay the agent for this attach
fil devbox -a         # don't, whatever the config says
```

The last of `-A` and `-a` wins. In your [config](config.md), `agent` is `off` (the default), `on` or `confirm` for a host. `-A` never lowers `confirm`: a flag that asks for the agent must not remove a question your config asked.

The client relays the agent at `SSH_AUTH_SOCK` as it was when `fil` started, never a path the host names. With no `SSH_AUTH_SOCK`, fil warns and carries on.

## The socket that survives reconnects

Every session on the host gets a stable `SSH_AUTH_SOCK`, `/tmp/fil-$UID/a-<major>-<rev>/<session>.sock`, whether or not a client relays. A program started while nobody relays finds the path and is refused, and works once a client with `-A` attaches. Because the path never changes for the life of the session, a reconnect, a takeover or a reattach doesn't leave a long-running shell with a dead agent socket, which is the usual way `git push` fails after you come back to tmux.

The socket exists only while the client holding the session relays its agent. Detached, or held by a client that doesn't relay, it is removed and a connection is refused at once. `agent_socket = false` in the host's [`daemon.toml`](daemon.md) drops the stable path altogether: sessions then have no `SSH_AUTH_SOCK`, and the agent is never relayed.

## What the host can do with it

The risk is the same as `ssh -A` while you are attached: root, or any process of your own user on the host, can ask your agent to sign. fil narrows it:

- **Only listing keys and signing** reach your agent (and ssh's `session-bind` and `query` extensions). Adding, removing, locking and the rest are refused.
- A signature is made only with a key your agent itself listed.
- `agent_keys` in your config limits the relay to the SHA256 fingerprints you list (`ssh-add -l` shows them); the others are neither listed nor signed with.
- `agent = "confirm"` asks you on your terminal before **every** signature, showing the key's type, fingerprint and comment. A question you don't answer within 30 seconds is refused. At most 4 questions wait at once and at most 6 are asked per minute, and after 3 denials in a row signatures are refused **without asking** for 60 seconds.
- Keys that your agent confirms per use (Touch ID, a hardware key) still prompt for each signature.
- **Destination-constrained keys fail closed.** Destination-constrained keys (`ssh-add -h`) can't be used through fil at all, for any destination: fil binds each agent connection so the agent refuses them. Use an unconstrained key, limited with `agent_keys`.
- A local agent that doesn't answer within 60 seconds is given up on, and the host gets a failure.

`~#` and `fil ports` show how many connections and requests the relay has served, and when it was last used. These are the current client's: they start over at a fresh attach.

## When the host's admin says no

The agent follows the host's server policy: `AllowAgentForwarding` and `DisableForwarding` in `sshd_config`, or `agent` in `/etc/fil/policy.toml`. If the policy refuses it, the attach says so once, and `fil ports` lists it under `REFUSED BY SERVER POLICY`. See [for host admins](policy.md).
