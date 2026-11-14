# fil

*Reprendre le fil*: to pick up the thread where you left off.

fil is a remote shell for development over SSH that keeps your place:

- **Reconnects transparently** after a dropped link, a sleeping laptop or a change of network, with your terminal's own native scrollback.
- **Keeps sessions** you can reattach after a local crash, or take over from another machine.
- **Holds port forwards open** across reconnects (`-L`, `-R`, `-D`), and shows you what each session holds with `fil ports`.
- **Connects directly over QUIC**, or over a plain SSH pipe when UDP is blocked, and moves between the two by itself.
- **Needs no root and no service** on the host: one program, run as you. It respects the host admin's sshd forwarding settings.

- **Shows what you type before the host echoes it**, as mosh does, when the round trip is long enough for that to help. This predictive local echo is on by default (`predict = "adaptive"`) from 0.2.0.

It has no panes: your terminal already does that.

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
- **Package signing key** (OpenPGP, RSA 4096; signs the `.rpm` packages and `SHA256SUMS.asc`; `.github/package-signing-key.asc`): `9EF0 B189 34B7 0552 0F88  80A5 81C0 98D1 5D1A EC26` (since 2026-11-08; it replaced `8BD8 1C03 EC58 8C5E FD85  04C8 8C14 2300 4B6D DA01`, which signed nothing released and is revoked: `.github/package-signing-key-revoked.asc`)

## Status

fil is an ssh/et drop-in: the client, the daemon, sessions that survive a reconnect or a takeover, both transports, port forwards, `fil ports`, the agent relay, server policy, and installing over ssh, as released in 0.1.0. 0.2.0 adds predictive local echo (below). The design and the milestones are in [DESIGN.md](DESIGN.md) (§15); the changes are in [CHANGELOG.md](CHANGELOG.md).

### Predictive echo (0.2.0)

While you type at a shell prompt, fil can draw the characters it expects the host to echo before the echo arrives, and replaces them with what the host really sends. `predict = "adaptive"`, the default, shows these guesses only when the round trip is above about 30 ms or for 30 s after an echo that took more than 250 ms, and underlines them while the round trip is above about 80 ms or in those 30 s. `predict = "always"` shows them at any round trip, and `predict = "off"` turns prediction off; set it in [`config.toml`](docs/user/config.md#predict), per host or for all, or for one run with `--predict off|adaptive|always`.

What it does not do:

- It predicts printable characters (ASCII and Latin-script characters), Backspace over what you just typed, and the left and right arrows within it. It never predicts Enter, Tab completion, history, other control keys, or anything that would wrap a line or scroll the screen.
- It stays off in full-screen programs that use the alternate screen (vim, less, htop, tmux), and for pasted text.
- Nothing is predicted once fil knows the host is at a password prompt. Four rare cases can still show typed keys, such as a password typed ahead of its prompt; they are listed in the [guide](docs/user/README.md#predictive-echo), and `predict = "off"` for a host is the way out.
- **Prediction after an interruption, changed in 0.2.1:** in 0.2.0, once something interrupted the run of guesses (Enter, Tab, Ctrl-W, a Backspace while keys were still on their way, reaching the end of the line), prediction stayed off until your typing slowed below about one key per round trip, about 7 keys a second at a 150 ms round trip and 2.5 at 400 ms. From 0.2.1 fil starts guessing again about one round trip later while you keep typing. What remains: typing steadily faster than about 12 keys a second gets no new guesses until it slows, and in fish or with zsh's autosuggestions fil guesses only while at most one key is on its way, so not while you type fast. This never shows anything wrong: fil just does not guess, and you see your keys when the host echoes them, as over ssh ([measurements](docs/m3/latency.md#coverage-at-long-round-trips-the-known-limit-020-ships-with)).
- Your keys go to the host as they would without it; what changes is your screen. With `predict` at `adaptive` or `always`, even on a fast link where no guess is shown, the host also acknowledges each stretch of typed input, which the [security and privacy](docs/user/security-privacy.md) page covers.

The goal and the measurements are in [`docs/m3/plan.md`](docs/m3/plan.md). In its scripted soak (1,000 seeds, output delivered as the daemon delivers it), programs that echo what you type show no visible misprediction, with one accepted exception for fish; some of the other measurements are still to be made.

PR numbers (#NN) in docs and comments refer to the repository's earlier history, which is archived.

**License:** [GPL-3.0-or-later](LICENSE).
