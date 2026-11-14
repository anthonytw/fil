# Changelog

All notable changes to fil. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and fil follows [Semantic Versioning](https://semver.org/) with the caveat that before 1.0 the configuration, the flags and the exit statuses may still change. A release that changes the wire (the `wire` number in `fil --version`) bumps the minor version, and both ends need the upgrade.

## [Unreleased]

### Changed

- **Predictive echo after an interruption.** In 0.2.0, once something interrupted the run of guesses (Enter, Tab, Ctrl-W, a Backspace while keys were still on their way, reaching the end of the line), prediction stayed off until your typing slowed below about one key per round trip. Now fil waits until the keys before the interruption have reached the host, about one round trip, then guesses the keys typed since and goes on, while you keep typing. Still not guessed: typing steadily faster than about 12 keys a second, until it slows, and fish or zsh's autosuggestions while more than one key is on its way. Nothing wrong is shown in either case; fil just does not guess.

### Fixed

- A guess erased by a later key's Backspace after a Right at the end of the line (readline's Right there does nothing) is no longer repainted over the erased cell until the next output arrives, and no longer ends the run of guesses.
- A guess the host's output drew and then erased in the same burst of output (an echo and a later key's erase, arriving together) is taken off at once, instead of staying on the screen over the erased cell for a quarter of a second.
- A Backspace typed over a space is no longer guessed: its erase shows nothing, so a later key's echo in the same output could count as a wrong guess.
- An echo that arrives in output the client handles just after the 250 ms late-echo grace, before its timer runs, now counts as an echo: it no longer takes the guesses after it off the screen until their own echoes bring them back (seen once in three runs at a bash prompt on a loaded host).

## [0.2.0] - 2026-11-12

Predictive local echo, on by default.

0.2.0 keeps 0.1.0's wire (`wire 1.14` in `fil --version`), so a 0.2.0 client goes on using a 0.1.0 daemon that is still running on a host. Install 0.2.0 on each host, then end your sessions there (or `fil kill` them), so the daemon's fixes below take effect; the guide's "Upgrading" section has the details.

A known limit, to be fixed in 0.2.1: once something interrupts the run of guesses (Enter, Tab, Ctrl-W, a Backspace while keys are still on their way, reaching the end of the line, fish's or zsh's autosuggestions), prediction stays off until your typing slows below about one key per round trip, about 7 keys a second at a 150 ms round trip and 2.5 at 400 ms. It never shows anything wrong: fil just does not guess, and you see your keys when the host echoes them, as over ssh. The guide's "Predictive echo" section has the details.

### Added

- **Predictive echo**, as in mosh: while you type at a shell or REPL prompt over a slow link, fil draws the characters it expects the host to echo, and replaces them with what the host sends. It predicts printable characters, Backspace over what you just typed and the left and right arrows; never Enter, Tab, history or other control keys, never a wrap or a scroll, nothing in full-screen programs (the alternate screen) and nothing pasted. Nothing is predicted once fil has seen a password prompt; four rare cases that can still draw typed keys are accepted and documented in the guide's "Predictive echo" section and on the security and privacy page.
- The `predict` setting (`off`, `adaptive` or `always`), in `[defaults]` and `[host.NAME]`, and `--predict MODE` for one run, over the file. The default is `adaptive`: guesses are shown only when the round trip is above about 30 ms or for 30 s after an echo that took more than 250 ms, and underlined while the round trip is above about 80 ms or in those 30 s. `fil config show` shows it with its source.
- `SECURITY.md`: how to report a vulnerability privately.

### Changed

- `remote = "auto"` (the default) now runs the install bootstrap first, on one ssh login: it starts the copy fil installed (under `~/.local/share/fil`), then `~/.local/bin/fil`, then a `fil` of this wire on the host's `PATH`, or else offers an install. Behind a forced command that allows only `fil server --stdio`, it retries that once. A login shell's raw "command not found" line for `fil` is replaced by fil's own message, and fil no longer suggests adding `remote = "installed"` after an install; `installed` still works and is no longer needed.
- `fil install` on a host that already has a `fil` of this wire on its `PATH` says so and installs nothing.
- The guide and the security and privacy page say that fil does not hide keystroke timing on the network, and that with `predict` at `adaptive` or `always` (even on a fast link) the daemon's echo acknowledgments repeat it in the traffic from the host; `off` is not a timing protection.

### Fixed

- `fil install` to a host whose login ends at once (a forced command that prints nothing) says why, instead of "Broken pipe".
- `fil kill` on a host where fil cannot read the process table returns once the session's processes have ended, instead of waiting out its whole grace period.
- `fil ls` reads a refusal that arrived before its request was sent, instead of failing on the send.
- A connection that arrives just after a forward's listener is paused stays queued for later instead of being taken.
- The daemon drops closed QUIC connections instead of keeping them.
- The daemon takes its socket-directory lock on the directory it checked, not on a second lookup of the same path.

### Security

- A program on the host that writes an endless OSC string (a title, a link, a clipboard write) no longer grows the daemon's or the client's memory without bound: such strings are dropped past 1 MiB, or 4 KiB for titles and links.
- The release signing key and the package signing key were replaced on 2026-11-08, and the old package key is published as revoked; the fingerprints are in the README.

## [0.1.0] - 2026-11-07

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

[Unreleased]: https://github.com/anthonytw/fil/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/anthonytw/fil/releases/tag/v0.2.0
[0.1.0]: https://github.com/anthonytw/fil/releases/tag/v0.1.0
