# fil

*Reprendre le fil*: to pick up the thread where you left off.

fil is a remote shell for development over SSH that keeps your place:

- **Reconnects transparently** after a dropped link, a sleeping laptop or a change of network, with your terminal's own native scrollback.
- **Keeps sessions** you can reattach after a local crash, or take over from another machine.
- **Holds port forwards open** across reconnects (`-L`, `-R`, `-D`), and shows you what each session holds with `fil ports`.
- **Connects directly over QUIC**, or over a plain SSH pipe when UDP is blocked, and moves between the two by itself.
- **Needs no root and no service** on the host: one program, run as you. It respects the host admin's sshd forwarding settings.

It has no panes: your terminal already does that. Predictive local echo, as in mosh, is planned for 0.2.0; 0.1.0 does not have it.

## Install

```sh
brew install anthonytw/tap/fil              # macOS, Linux
sudo apt install ./fil_<version>_amd64.deb  # Debian, Ubuntu (from a GitHub Release)
cargo install --locked fil-shell            # from source; the crate is fil-shell, the command is fil
```

Fedora, Arch, Nix, the release archives and how to check them are in the [install guide](docs/user/install.md). fil goes on the host too: `fil install HOST` puts it there over your ssh login, or the first `fil HOST` offers to.

## Quick start

```sh
fil devbox                  # attach to (or create) the session "main" on devbox
~.                          # (after Enter) detach; the session keeps running
fil devbox                  # come back, from this machine or another
fil devbox build -L 5173    # session "build", with localhost:5173 forwarded
fil ls devbox               # list the sessions
fil ports devbox            # what each session holds and what is listening
```

See the [guide](docs/user/README.md): [quick start](docs/user/quick-start.md), [sessions and takeover](docs/user/sessions.md), [transports and firewalls](docs/user/transports.md), [forwards](docs/user/forwards.md), [configuration](docs/user/config.md), [for host admins](docs/user/policy.md), [security and privacy](docs/user/security-privacy.md), and [troubleshooting](docs/user/troubleshooting.md). A release also installs the manual pages `fil(1)`, `fil-config(5)` and `fil-policy(5)`, and completions for bash, zsh and fish.

## Release keys

Check these fingerprints against the copies in the repository before you trust a release:

- **Release signing key** (SSH, Ed25519; signs the release tags and `SHA256SUMS`; `.github/release-signers`): `SHA256:Q7KfBHSjAFRimlv46rQuCGxsQF0NHQgLTpBcekwvnp0` (since 2026-11-08; the retired `SHA256:sEx4DtGI9gdaVhgsYWwuIC0xwkYJ8BLIVhVktPHINvQ` signed only the v0.1.0 tag)
- **Package signing key** (OpenPGP, RSA 4096; signs the `.rpm` packages and `SHA256SUMS.asc`; `.github/package-signing-key.asc`): `9EF0 B189 34B7 0552 0F88  80A5 81C0 98D1 5D1A EC26` (since 2026-11-08; it replaced `8BD8 1C03 EC58 8C5E FD85  04C8 8C14 2300 4B6D DA01`, which signed nothing released)

## Status

fil is approaching its first release, 0.1.0: an ssh/et drop-in. The client, the daemon, sessions that survive a reconnect or a takeover, both transports, port forwards, `fil ports`, the agent relay, server policy, and installing over ssh work. The design and the milestones are in [DESIGN.md](DESIGN.md) (§15); the changes are in [CHANGELOG.md](CHANGELOG.md).

PR numbers (#NN) in docs and comments refer to the repository's earlier history, which is archived.

**License:** [GPL-3.0-or-later](LICENSE).
