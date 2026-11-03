# fil

*Reprendre le fil*: to pick up the thread where you left off.

fil is a remote shell for development over SSH that keeps your place:

- **Reconnects transparently**, with your terminal's own native scrollback.
- **Keeps sessions** you can reattach after a local crash, or take over from another machine.
- **Holds port forwards open** across reconnects, and shows you what each session holds.
- **Optional mosh-style predictive echo** for laggy links.
- **Connects directly over QUIC**, or over a plain SSH pipe when UDP is blocked.

It has no panes: your terminal already does that.

**Status:** in development. The link simulator, the prediction overlay, sessions, the client and both transports are built and being hardened; there is no release yet, so there's nothing to install. See [DESIGN.md](DESIGN.md).

**License:** [GPL-3.0-or-later](LICENSE).
