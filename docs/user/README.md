# The fil guide

fil is a remote shell for development over SSH. It keeps your place: the connection can drop, your laptop can sleep or change networks, and your shell, its output and your port forwards are still there when you come back. This guide is for people who use fil and for the admins of the hosts they use it on.

This is the guide for **0.2.0**, which adds [predictive echo](#predictive-echo) to the ssh/et drop-in that 0.1.0 was. Where something is new in 0.2.0, the page says so.

## Start here

1. [Install](install.md) fil on your machine, and on each host you connect to.
2. [Quick start](quick-start.md): your first session, in five minutes.
3. [Sessions, detach and takeover](sessions.md): how sessions live on after the client goes away, and what happens when a second machine attaches.

## Using fil

- [Reconnects and transports](transports.md): the ssh pipe, direct QUIC over UDP, and what a firewall changes.
- [Forwards and `fil ports`](forwards.md): `-L`, `-R`, `-D`, and the view of what each session holds.
- [The agent](agent.md): relaying your ssh-agent to a session, and what that risks.
- [Predictive echo](#predictive-echo), below: what you see while you type over a slow link.
- [fil compared with ssh, ET and mosh](compared.md).

## Predictive echo

New in 0.2.0. Over a slow link, every key you type takes a round trip before it appears, because the host echoes it. fil can draw the character in your terminal at once, as a guess, and replace it with what the host really sends when that arrives, as mosh does.

**What you see.** With the default, `predict = "adaptive"`, fil guesses only when the round trip is above about 30 ms (and stops below about 20 ms), so on a fast link you see no difference. For 30 s after an echo that took more than 250 ms from its key, it shows guesses even on a fast link. While the round trip is above about 80 ms, or in those 30 s, the guesses are **underlined**, so you can tell them from what the host has confirmed; the underline goes when the echo arrives. On a link in between, guesses are drawn plain. A guess that turns out wrong is replaced by what the host sent, usually within one round trip.

**What it predicts.** Printable characters (ASCII and Latin-script characters) at a shell or REPL prompt, Backspace over what you just typed, and the left and right arrows (and Ctrl-B, Ctrl-F) within what you just typed. It never predicts Enter, Tab completion, history, Ctrl-R, Ctrl-U, Ctrl-W or other control keys, never draws anything that would wrap a line or scroll the screen, and stays off in full-screen programs that use the alternate screen (vim, less, htop, tmux). Your keys go to the host exactly as they would without prediction.

**A known limit in 0.2.0** (to be fixed in 0.2.1). Once something interrupts the run of guesses, prediction stays off until your typing slows below about one key per round trip: about 7 keys a second at a 150 ms round trip and 2.5 at 400 ms, slower than many people type. What interrupts it: Enter, Tab, Ctrl-W, a Backspace while keys are still on their way to the host, reaching the end of the line, and fish's or zsh's autosuggestions. Within a line that starts with nothing in flight, prediction works. This limit never shows anything wrong: fil just does not guess, and you see your keys when the host echoes them, as over ssh. The measurements are in [the latency cells](../m3/latency.md#coverage-at-long-round-trips-the-known-limit-020-ships-with).

**Turning it off.** In [`config.toml`](config.md#predict), for one host or for all:

```text
[host.devbox]
predict = "off"
```

or for one run, `fil devbox --predict off`. `predict = "always"` shows guesses at any round trip. `fil config show devbox` shows the setting in effect and where it came from.

**Password prompts.** Nothing is predicted while the host's terminal is at a password prompt (the mode `sudo`, `ssh`, `passwd`, `read -s` and `getpass` use: canonical, with echo off), and anything already drawn is taken back when fil learns of the prompt. Four rare cases can still draw keys you type; they were accepted as the price of predicting in shells, whose line editors turn echo off too:

1. A raw-mode program that starts reading a secret on an ordinary key, without changing the terminal mode or drawing a prompt first. The keys typed before fil sees that they are not echoed are drawn: about one round trip and 50 ms.
2. A password typed ahead of its prompt, such as `sleep 3; read -s` with the password typed during the `sleep`. The keys typed in the round trip before fil learns of the prompt are drawn.
3. Output that equals the typed keys, such as an asterisk mask's `*` when you type `*`, or a PIN prompt that echoes `1` for a typed `1`: fil takes it as an echo.
4. A secret prompt drawn only to the right of the cursor, or only by changing colors or other attributes, or drawn and erased again before fil looks.

A guess once drawn has reached your terminal, its scrollback and anything recording or sharing your screen, even if fil erases it at once. If that matters on a host, set `predict = "off"` for it. The [security and privacy](security-privacy.md) page has the rest.

**Pasting.** Pasted text is never predicted: it goes to the host as is, and you see it when the host echoes it. fil recognizes a paste by your terminal's bracketed-paste markers, or, when the program on the host has not turned bracketed paste on, by its size (a read of 64 bytes or more, and what follows it within a few milliseconds). After a paste, as after Enter, fil waits for the host to echo the first key you type before it shows guesses again.

## Configuration

- [`config.toml`](config.md): the client's settings, per host or for all. `fil --config FILE` (or `$FIL_CONFIG`) reads another file instead; a file named that way must exist.
- [`daemon.toml`](daemon.md): the daemon's settings on the host you connect to.
- [For host admins: `/etc/fil/policy.toml`](policy.md) and the way fil mirrors sshd's forwarding settings.

## Trust and trouble

- [Security and privacy](security-privacy.md), in plain words.
- [Troubleshooting](troubleshooting.md).

## Manual pages

A release archive carries `fil(1)`, `fil-config(5)` (the `config.toml` page) and `fil-policy(5)` (the `policy.toml` page), and completions for bash, zsh and fish. The Homebrew, `.deb` and `.rpm` packages install them in the usual places. A build from source with `cargo install` or Nix has none of them; this guide is the same text.
