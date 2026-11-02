# fil: guidance for Claude

fil is a remote shell for development over SSH:

- reconnect with native scrollback
- sessions you can reattach or take over from another machine
- port forwards that survive reconnects
- optional predictive echo
- two transports: a pipe (ssh) and direct QUIC

## Start here

- **`DESIGN.md` is the source of truth.**
  - Items marked **(decided)** are the owner's decisions. Build on them; don't reopen them.
  - Everything else is a proposal and can change. When one changes, update `DESIGN.md` in the same PR.
- **Open questions** are in `DESIGN.md` §16. **Milestones** are in §15. Work goes riskiest first, starting with the M0 spikes.
- **Ask before starting unrequested work.** Propose it and wait for the owner's OK. A question like "how hard would X be?" wants a feasibility answer, not an implementation.

## Conventions

- **Rust:** a cargo workspace, laid out as in `DESIGN.md` §13. Before any PR, these must pass:
  - `cargo fmt --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo test`
- **Logging:** `tracing`, with JSON output.
- **License:** GPL-3.0-or-later. Every dependency must be GPL-3.0-compatible:
  - fine: MIT, Apache-2.0, BSD, ISC, MPL-2.0, LGPL, GPL-3.0
  - not allowed: GPL-2.0-only, or anything proprietary
- **Diagrams in Markdown are Mermaid, never ASCII art.**
  - Render-check every new or changed diagram before committing, for example with `npx @mermaid-js/mermaid-cli`.
  - Terminal-output mockups (like `fil ports` in `DESIGN.md`) stay as plain code blocks.
- **The security invariants in `DESIGN.md` §11 are requirements:** the fixed remote command, no secrets at rest, peer-UID checks, pinned certificates. A change that weakens any of them needs the owner's sign-off.

## Delivery

- **Workflow:** branch, then PR, then squash merge, using `gh`. The repo is on GitHub (private for now). One task per PR.
- **Signing:** the global git config SSH-signs commits. Don't disable it.
- **The repo will go public.** Never commit secrets, IP addresses or personal data.
