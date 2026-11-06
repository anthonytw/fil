# Changelog

All notable changes to fil. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and fil follows [Semantic Versioning](https://semver.org/) with the caveat that before 1.0 the configuration, the flags and the exit statuses may still change. A release that changes the wire (the `wire` number in `fil --version`) bumps the minor version, and both ends need the upgrade.

## [0.1.0] - unreleased

The first release: an ssh/et drop-in. It has no predictive echo and no `predict` setting; that is planned for 0.2.0.

### Added

- Sessions that survive a dropped link, a sleeping laptop and a change of network, resumed where the stream stopped, with the terminal's native scrollback.
- Named sessions that outlive the client: reattach after a crash, detach with `~.` or `fil detach`, take a session over from another machine, `fil ls`, `fil kill`.
- Two transports with automatic fallback: the ssh pipe and direct QUIC over UDP (60000-61000 by default), certificate pinned over the ssh login. `--transport auto|pipe|direct`.
- Port forwards `-L`, `-R` and `-D` that belong to the session and survive reconnects; `fil forward` to add and remove them; `fil ports` for what each session holds and what is listening, with `--json` and `--probe`.
- Opt-in relay of the local ssh-agent (`-A`, `-a`), with a stable `SSH_AUTH_SOCK` in every session, key limits and per-signature confirmation.
- Server policy for host admins: a mirror of sshd's forwarding settings and `/etc/fil/policy.toml`.
- `fil install` and the first-attach offer put fil on a host over the ssh login, into `~/.local/share/fil`; `--check` and `--remove`.
- `config.toml` for the client and `daemon.toml` for the daemon.
- Release archives for Linux (x86_64, aarch64) and macOS (universal), `.deb` and `.rpm` packages, Homebrew, AUR and Nix recipes, a crates.io package (`fil-shell`), signed checksums, and reproducible builds.
- This guide (`docs/user/`), the manual pages `fil(1)`, `fil-config(5)` and `fil-policy(5)`, and completions for bash, zsh and fish.

## Development history (archived)

Before 0.1.0 fil was developed in milestones M0 to M4, and that history is archived (the repository starts again from one initial commit). M0 proved the risky parts (terminal restore fidelity, the emulator and the transports), M1 built the client, the daemon and both transports, M2 the port forwards, `fil ports` and the agent relay, and M3 the predictive echo that is held for 0.2.0. M4 prepared the release: installing over ssh, configuration, this documentation, packaging and the license. The pull request numbers in the repository's documents and comments refer to that history.

[0.1.0]: https://github.com/anthonytw/fil/releases/tag/v0.1.0
