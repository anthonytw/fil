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
- **Open questions** are in `DESIGN.md` §16. **Milestones** are in §15. Work goes riskiest first. M0 and M1 are done; M2 is in progress (`docs/m2/plan.md`).
- **Ask before starting unrequested work.** Propose it and wait for the owner's OK. A question like "how hard would X be?" wants a feasibility answer, not an implementation.

## Conventions

- **Rust:** a cargo workspace, laid out as in `DESIGN.md` §13. Crates live in `crates/`. The toolchain is pinned in `rust-toolchain.toml`.
- **Tests:** keep them proportionate. Unit-test the logic that is easy to get wrong, such as frame parsing, sequence numbers and the snapshot writer. Heavy harnesses, like the fidelity suite and randomized overlay runs, are commands you run on demand, not part of the merge gate.
- **Logging:** `tracing`, with JSON output.
- **POSIX calls:** library and binary code uses `rustix`. `nix` is for tests and examples only. The one exception is `fil-session`, which keeps `nix` because portable-pty's API hands over nix types (the PTY master's termios) and its signal and uid calls were not converted separately; new code there uses rustix where it can.
- **License:** GPL-3.0-or-later. Every dependency must be GPL-3.0-compatible:
  - fine: MIT, Apache-2.0, BSD, ISC, MPL-2.0, LGPL, GPL-3.0
  - not allowed: GPL-2.0-only, or anything proprietary
- **Diagrams in Markdown are Mermaid, never ASCII art.**
  - Render-check every new or changed diagram before committing, for example with `npx @mermaid-js/mermaid-cli`.
  - Terminal-output mockups (like `fil ports` in `DESIGN.md`) stay as plain code blocks.
- **The wire has a revision.** `PROTOCOL_REVISION` in `crates/fil-proto/src/limits.rs` **must change whenever any message encoding changes**: a field or variant of a control message, the data segment, the QUIC stream layout, a limit a decoder enforces. Builds of different revisions refuse each other at the hello, and the revision is in the daemon's socket name so their daemons coexist (`DESIGN.md` §5.3, §8.5). `crates/fil-proto/tests/golden/wire.txt` is a committed encoding of every message, tagged with the revision; `just ci` fails if the encoding changes without a bump. To change the wire: bump `PROTOCOL_REVISION`, run `just wire-golden`, commit the golden with the change. The golden also covers every decoder limit and the widest value of each field, and its `[history]` makes two PRs that bump to the same revision conflict in git: when it does, take main's, bump past it and regenerate. `target/*/fil` after a `cargo test` or an example build has the test hooks (`FIL_TEST_*`) in it and is not a release binary; the recipes that install it (`pi-e2e`, `dropin`) build it in an invocation of its own and `just no-test-hooks` checks. The preamble, the hello, the opening ping and pong (`fil_proto::opening`: fixed bytes the relay routes by, not control messages) and the frame layout are *frozen* within a `PROTOCOL_MAJOR`: a `[frozen]` line in the golden may be added (with a revision) and never changed, so a relay of any revision from 3 on can route a client of any other. An added line may only pin something no older peer has to read (a new probe, a new fact); a new form of the opening or the hello that a peer sends or must accept needs a new `PROTOCOL_MAJOR`.
- **The security invariants in `DESIGN.md` §11 are requirements:** the fixed remote command, no secrets at rest, peer-UID checks, pinned certificates. A change that weakens any of them needs the owner's sign-off.

## Delivery

- **Workflow:** branch, then PR, then squash merge, using `gh`. The repo is on GitHub and public. One task per PR.
- **The merge gate is local.** There is no required CI on GitHub. Before merging, run `just ci` on the branch, rebased on current `main`. It runs `cargo fmt --check`, `clippy -D warnings`, a default-features `cargo check -p fil`, `cargo test` and `cargo deny` (the license, source and ban policy and the RustSec advisories, including yanked crates, in `deny.toml`; the advisory database is fetched, so it needs network access; `FIL_DENY_FLAGS=--offline just ci` checks against the cached copy instead, which can be stale, so an offline pass is not a merge pass). It also fails if the vt100 emulator backend is back in the release binary's dependency graph (`just release-deps`). Say in the PR that it passed.
- **Linux check:** a manual GitHub workflow (`gh workflow run linux-check -f ref=BRANCH`) runs `just ci` and the on-demand harnesses on Linux. Use it when a change touches PTYs, termios, sockets or anything else platform-specific. It is a check, not a gate. `-f suite=realnet` runs the real-network harness instead (real OpenSSH in Docker, NAT and `tc netem` in network namespaces, on static x86_64 musl binaries; `just realnet` on a Linux box, see `docs/m1/realnet.md`); `-f suite=aarch64` builds the static aarch64 binary for the lab Pis (`docs/m1/pi.md`, `just pi-e2e`); `-f suite=all` runs all three.
- **macOS check:** `gh workflow run macos-check -f ref=BRANCH` (`-f suite=check|remote|all`) is the same kind of manual check on GitHub's macOS runners (Apple silicon and Intel): `just ci` and the harnesses on macOS, and macOS as the remote host over a real ssh login (`just macos-remote`, `tests/macos/`). Use it when a change touches anything platform-specific on macOS.
- **Signing:** the global git config SSH-signs commits. Don't disable it.
- **The repo is public.** Everything pushed is world-visible, including PR branches and their commits, which GitHub keeps even after a squash merge. Never push secrets, IP addresses or personal data, not even in a work-in-progress commit.
- **Reviews and sweeps.**
  - Every PR gets an independent review, and the review always covers **user privacy** (DESIGN.md §11.1) alongside correctness and security.
  - Every milestone ends with a whole-tree **sweep** (DESIGN.md §15): independent audits for redundancy, efficiency, user privacy, security and bugs, then fix PRs, then a verification pass.
  - Privacy here means what fil reveals about the people using it, not just keeping our own secrets out of the repo.
