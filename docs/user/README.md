# The fil guide

fil is a remote shell for development over SSH. It keeps your place: the connection can drop, your laptop can sleep or change networks, and your shell, its output and your port forwards are still there when you come back. This guide is for people who use fil and for the admins of the hosts they use it on.

This is the guide for **0.1.0**. fil 0.1.0 is an ssh/et drop-in without predictive echo; that feature is planned for 0.2.0.

## Start here

1. [Install](install.md) fil on your machine, and on each host you connect to.
2. [Quick start](quick-start.md): your first session, in five minutes.
3. [Sessions, detach and takeover](sessions.md): how sessions live on after the client goes away, and what happens when a second machine attaches.

## Using fil

- [Reconnects and transports](transports.md): the ssh pipe, direct QUIC over UDP, and what a firewall changes.
- [Forwards and `fil ports`](forwards.md): `-L`, `-R`, `-D`, and the view of what each session holds.
- [The agent](agent.md): relaying your ssh-agent to a session, and what that risks.
- [fil compared with ssh, ET and mosh](compared.md).

## Configuration

- [`config.toml`](config.md): the client's settings, per host or for all. `fil --config FILE` (or `$FIL_CONFIG`) reads another file instead; a file named that way must exist.
- [`daemon.toml`](daemon.md): the daemon's settings on the host you connect to.
- [For host admins: `/etc/fil/policy.toml`](policy.md) and the way fil mirrors sshd's forwarding settings.

## Trust and trouble

- [Security and privacy](security-privacy.md), in plain words.
- [Troubleshooting](troubleshooting.md).

## Manual pages

A release archive carries `fil(1)`, `fil-config(5)` (the `config.toml` page) and `fil-policy(5)` (the `policy.toml` page), and completions for bash, zsh and fish. The Homebrew, `.deb` and `.rpm` packages install them in the usual places. A build from source with `cargo install` or Nix has none of them; this guide is the same text.
