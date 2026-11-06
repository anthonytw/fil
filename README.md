# fil

*Reprendre le fil*: to pick up the thread where you left off.

fil is a remote shell for development over SSH that keeps your place:

- **Reconnects transparently**, with your terminal's own native scrollback.
- **Keeps sessions** you can reattach after a local crash, or take over from another machine.
- **Holds port forwards open** across reconnects, and shows you what each session holds.
- **Optional mosh-style predictive echo** for laggy links.
- **Connects directly over QUIC**, or over a plain SSH pipe when UDP is blocked.

It has no panes: your terminal already does that.

**Status:** in development. The `fil` client, the daemon, sessions that survive a reconnect or a takeover, and both transports (the ssh pipe and direct QUIC) work, and M1 is done. Port forwards (`-L`, `-R`, `-D`, `fil forward`), `fil ports` and the agent relay are built, and M2 is done; predictive echo is not built yet, and there is no release, so there's nothing to install. See [DESIGN.md](DESIGN.md) (milestones in §15).

PR numbers (#NN) in docs and comments refer to the repository's earlier history, which is archived.

## Release keys

Check these fingerprints against the copies in the repository before you trust a release:

- **Release signing key** (SSH, Ed25519; signs the release tags and `SHA256SUMS`; `.github/release-signers`): `SHA256:sEx4DtGI9gdaVhgsYWwuIC0xwkYJ8BLIVhVktPHINvQ`
- **Package signing key** (OpenPGP, RSA 4096; signs the `.rpm` packages and `SHA256SUMS.asc`; `.github/package-signing-key.asc`): `8BD8 1C03 EC58 8C5E FD85  04C8 8C14 2300 4B6D DA01`

**License:** [GPL-3.0-or-later](LICENSE).
