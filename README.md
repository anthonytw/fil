# fil

*Reprendre le fil*: to pick up the thread where you left off.

fil is a remote shell for development over SSH that keeps your place:

- **Reconnects transparently**, with your terminal's own native scrollback.
- **Keeps sessions** you can reattach after a local crash, or take over from another machine.
- **Holds port forwards open** across reconnects, and shows you what each session holds.
- **Optional mosh-style predictive echo** for laggy links.
- **Connects directly over QUIC**, or over a plain SSH pipe when UDP is blocked.

It has no panes: your terminal already does that.

**Status:** design stage. There's nothing to install yet. See [DESIGN.md](DESIGN.md).

**License:** GPL. The exact version is still to be decided.
