# Changelog

All notable changes to fil. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and fil follows [Semantic Versioning](https://semver.org/) with the caveat that before 1.0 the configuration, the flags and the exit statuses may still change. A release that changes the wire (the `wire` number in `fil --version`) bumps the minor version, and both ends need the upgrade.

## [Unreleased]

### Added

- **An installer script, `fil-install.sh`,** in every release from now on (the first is the release after 0.3.0). `curl -fsSL https://github.com/anthonytw/fil/releases/latest/download/fil-install.sh | sh` (or download it, read it, then `sh fil-install.sh --version X.Y.Z`) checks the release's signed `SHA256SUMS` against the release key written in the script, checks the archive for your machine against it, and copies `fil` into `~/.local/bin`, with the Linux programs `fil install` uploads beside it. No root; it stops before installing anything if a check fails. The release's `SHA256SUMS` lists it, so it is signed like every other file. The [install guide](docs/user/install.md) has the details.
- **fil's own apt and dnf repositories,** from the release after 0.3.0: the release's `.deb` and `.rpm` packages, with indexes signed by the package signing key, served from the project site. The install guide has the one-time setup.
- **A security audit process** ([`SECURITY.md`](SECURITY.md) "Audits", [`docs/audits/`](docs/audits/README.md)): a release audit for every minor release and a full audit every six months, reviewed by AI models with independent verification and published after the fixes ship; a severity scale and when a CVE is requested; and a weekly scan of the locked dependencies against the RustSec and OSV advisory databases. The first full audit runs before 1.0.
- **`fil -N HOST`: forwards only, with no terminal**, as `ssh -N`. It holds the session's `-L`, `-R` and `-D` forwards (and the agent relay with `-A`) until you stop it with Ctrl-C, reconnecting like any fil, and works in the background or from a script. The session is `tunnel` unless you name one, so it never asks to take your shell's `main`. It needs a forward (a flag or the config's `forward`) or `-A`, or it exits with status 2. It asks nothing: not whether to install fil on a host that has none (with `install = "ask"` it says to run `fil install HOST`; `install = "always"` installs), and a forward the host asks for, or a signature `agent = "confirm"` would ask about, is refused. A session held by a disconnected client (an earlier tunnel that died without detaching) is taken over, and said so; one held by a connected client is left alone (status 1) unless `--take-over` is given, which unattended tunnels should pass. Ctrl-C detaches it and exits 0 (255 before it is attached, as `ssh -N`), and the host hears the detach over the pipe too. A signal it was started with ignored stays ignored, so `nohup fil -N ... &` survives closing the terminal.
- `fil ls` says when the host's daemon is a newer fil than yours, with the command that upgrades yours (read from the daemon's greeting, which fil already gets; nothing more is asked of the host).
- **`fil kill --outdated HOST`** ends the sessions whose daemon is an older fil than yours, after an upgrade (a security fix, say). It lists them first, with each one's daemon version and the jobs still running in it, and asks; without a terminal it ends nothing unless you pass `--yes`. A daemon from before this check (`wire 1.16`) cannot report its sessions' jobs: they are listed as "not checked", and still ended on `y`. It never reports a host as clean when it could not check all of it: a daemon it could not reach (another wire's, such as a 0.x daemon; one whose list it could not get) is named, and the status is 1 then, as when you decline or a kill fails. With an older fil on the host (`remote = "plain"`) it ends nothing and says to upgrade the host's fil first. Each daemon and each session is an ssh connection of its own: with a password login, use `ControlMaster`.
- **Every session on a host is reachable after an upgrade** (decision D2). `fil ls HOST` lists the sessions of every running daemon of your fil's wire family in one table (one connection to each daemon; its FIL column says which holds each), and `fil HOST SESSION`, `fil kill`, `fil detach`, `fil forward` and `fil ports` with a session reach that session on whichever daemon holds it. A session that exists nowhere is made on the newest daemon, as before. This needs the fil that runs on the host to be this version or later: the install command always runs your own version there; with `remote = "plain"` and an older fil on the host, connections go to its default daemon as before.

### Changed

- **The wire is now the 1.x wire (`wire 1.17`; `wire 1.16` is its first form), and the last one that needs both ends upgraded together.** From `wire 1.16` on, any two fil builds of the same major talk to each other, whichever is newer: a newer build uses a new feature only when the other side has it too, and skips what it does not know. Builds of `wire 1.15` and older still talk only to their own wire, so upgrade both ends to this version once.
- **Each version of fil runs its own daemon on a host** (decision D2). A daemon's socket is now named for its build as well as its wire (`/tmp/fil-$UID/fil-<major>-<rev>-v<version>.sock`), so a new version's first connection starts its daemon beside the old one even when the wire is unchanged. New sessions go to the newest daemon of the client's wire, where from `wire 1.16` on every 1.x wire counts as one (an older `fil` on the host connects to it too, and a 1.x client is never refused by a 1.x host), and an older daemon exits as soon as its last session ends, so daemon-side fixes reach every new session at once. `fil ls` shows each session's daemon version (a new FIL column) and names the daemons of other wires running on the host with their versions; the sessions of an older daemon of the same wire family stay reachable (above), so there is no need to end sessions before upgrading a host. A daemon from an earlier version keeps its wire: a new daemon of the same wire does not start beside it, and connections go to it as before.
- The daemons of one wire revision share its lock file (`fil-<major>-<rev>.lock`): the first takes it and each later one is handed the first one's open file over a local socket, so `fil install --remove` and the install's cleanup see it held from the first daemon's start to the last one's exit and never remove a copy a running daemon needs. Each daemon has a lending socket (`…-v<version>.lend`, mode 0600) for this, in the socket directory.
- A session's `SSH_AUTH_SOCK` directory is named for its daemon's build (`/tmp/fil-$UID/a-<hash>/`), so same-named sessions of two daemons never share an agent socket.
- When fil leaves a session alone because another client holds it, the message says "(disconnected)" when that client's link is down, as the question does.
- **`fil --version` says how to upgrade.** Its second line names how this copy was installed and the command that upgrades it (Homebrew, cargo, the `.deb` or `.rpm`, the AUR, Nix, a `fil install` copy on a host, `~/.local/bin`, or a copy by hand, with its path, where `~` stands for your home directory so a pasted `--version` does not name you). fil works this out from its own path and the package manager's files, with no network and without running anything; it still never updates itself or checks for updates. The first line is unchanged; the legal notices follow.

- `offline_input.confirm` reads its duration as the rest of the config file does: `h` (hours) is accepted too, and a value too large is an error instead of meaning "never".
- The daemon's log keeps less about you at its default level: a panic is logged with where it happened, and its message (which can quote the text it was handling) only at debug level; the daemon's socket path (which holds your user id) is logged at debug level too.

### Fixed

- A daemon with nothing to do now exits after `idle_exit_secs` (five minutes by default). The idle wait started over at every 15-second file check, so with any idle time longer than 15 seconds the daemon never exited on its own.

### Fixed

- A daemon with nothing to do now exits after `idle_exit_secs` (five minutes by default). The idle wait started over at every 15-second file check, so with any idle time longer than 15 seconds the daemon never exited on its own.

## [0.3.0] - 2026-11-14

Ask before taking over a session, and predictive echo that recovers after an interruption.

0.3.0 changes the wire (`wire 1.15` in `fil --version`; 0.1.0 and 0.2.0 are `wire 1.14`), so install it on your machine and on each host: a 0.3.0 client does not talk to a 0.2.0 daemon. Daemons of different wires run side by side, so nothing breaks: your 0.2.0 sessions stay on the old daemon until they end (the 0.3.0 `fil` does not list or reach them; the 0.2.0 `fil` still does), and a 0.3.0 client starts its own. The guide's "Upgrading" section has the details.

### Added

- `--take-over` and the `takeover` setting (`"ask"`, the default, or `"always"`), in `[defaults]` and `[host.NAME]`, to take a session over without being asked. `fil config show` shows it with its source.

### Changed

- **Attaching to a session that another client holds asks first.** fil says who has it and since when (`fil: "build" is attached from laptop since 2026-11-13 14:02:00. Take it over?  y = yes, n = no`, with `(disconnected)` for a client whose link is down) and takes it over only on `y`, and only from the client it named: if the session changes hands while you decide, it asks again. `n` leaves the other client alone and exits with status 1. Before, such an attach always took the session over. `--take-over` or `takeover = "always"` restores that. A session nobody is attached to, a reconnect and taking a session back after a takeover never ask.
- The "taken over" message and the take-back question give the time in your local time instead of UTC.
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

[Unreleased]: https://github.com/anthonytw/fil/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/anthonytw/fil/releases/tag/v0.3.0
[0.2.0]: https://github.com/anthonytw/fil/releases/tag/v0.2.0
[0.1.0]: https://github.com/anthonytw/fil/releases/tag/v0.1.0
