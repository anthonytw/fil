# fil

*Reprendre le fil*: to pick up the thread where you left off.

fil is a remote shell for development over SSH that keeps your place:

- **Reconnects transparently**, with your terminal's own native scrollback.
- **Keeps sessions** you can reattach after a local crash, or take over from another machine.
- **Holds port forwards open** across reconnects, and shows you what each session holds.
- **Optional mosh-style predictive echo** for laggy links.
- **Connects directly over QUIC**, or over a plain SSH pipe when UDP is blocked.

It has no panes: your terminal already does that.

**Status:** in development. The `fil` client, the daemon, sessions that survive a reconnect or a takeover, and both transports (the ssh pipe and direct QUIC) work, and M1 is done. Port forwards (`-L`, `-R`, `-D`, `fil forward`), `fil ports` and the agent relay are built and in their sweep (M2); predictive echo is not built yet, and there is no release, so there's nothing to install. See [DESIGN.md](DESIGN.md) (milestones in §15).

PR numbers (#NN) in docs and comments refer to the repository's earlier history, which is archived.

**License:** [GPL-3.0-or-later](LICENSE).
