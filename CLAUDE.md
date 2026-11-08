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
- **Open questions** are in `DESIGN.md` §16. **Milestones** are in §15. Work goes riskiest first. M0, M1 and M2 are done (`docs/m2/plan.md` is M2's historical work order); M3 is in progress and targets 0.2.0, and M4 is built with its sweep in progress (`docs/m3/plan.md`, `docs/m4/plan.md`).
- **Ask before starting unrequested work.** Propose it and wait for the owner's OK. A question like "how hard would X be?" wants a feasibility answer, not an implementation.

## Conventions

- **Rust:** a cargo workspace, laid out as in `DESIGN.md` §13. Crates live in `crates/`. The toolchain is pinned in `rust-toolchain.toml`.
- **Tests:** keep them proportionate. Unit-test the logic that is easy to get wrong, such as frame parsing, sequence numbers and the snapshot writer. Heavy harnesses, like the fidelity suite and randomized overlay runs, are commands you run on demand, not part of the merge gate. A test whose pass or fail depends on real elapsed time is named `wallclock_*` and runs alone in `just test-wallclock` (`docs/dev/timing-tests.md`: P and T bounds, `fil_testkit::timing::Stalls`); a bound that is a DESIGN.md claim is never relaxed to make CI pass.
- **Logging:** `tracing`, with JSON output.
- **POSIX calls:** library and binary code uses `rustix`. `nix` is for tests and examples only. The one exception is `fil-session`, which keeps `nix` because portable-pty's API hands over nix types (the PTY master's termios) and its signal and uid calls were not converted separately; new code there uses rustix where it can.
- **License:** GPL-3.0-or-later. Every dependency must be GPL-3.0-compatible:
  - fine: MIT, Apache-2.0, BSD, ISC, MPL-2.0, LGPL, GPL-3.0
  - not allowed: GPL-2.0-only, or anything proprietary
- **Diagrams in Markdown are Mermaid, never ASCII art.**
  - Render-check every new or changed diagram before committing, for example with `npx @mermaid-js/mermaid-cli`.
  - Terminal-output mockups (like `fil ports` in `DESIGN.md`) stay as plain code blocks.
- **The wire has a revision.** `PROTOCOL_REVISION` in `crates/fil-proto/src/limits.rs` **must change whenever any message encoding changes**: a field or variant of a control message, the data segment, the QUIC stream layout, a limit a decoder enforces. Builds of different revisions refuse each other at the hello, and the revision is in the daemon's socket name so their daemons coexist (`DESIGN.md` §5.3, §8.5). `crates/fil-proto/tests/golden/wire.txt` is a committed encoding of every message, tagged with the revision; `just ci` fails if the encoding changes without a bump. To change the wire: bump `PROTOCOL_REVISION`, run `just wire-golden`, commit the golden with the change. The golden also covers every decoder limit and the widest value of each field, and its `[history]` makes two PRs that bump to the same revision conflict in git: when it does, take main's, bump past it and regenerate. `target/*/fil` after a `cargo test` or an example build has the test hooks (`FIL_TEST_*`) in it and is not a release binary; the recipes that install it (`pi-e2e`, `dropin`) build it in an invocation of its own and `just no-test-hooks` checks. The preamble, the hello, the opening ping and pong (`fil_proto::opening`: fixed bytes the relay routes by, not control messages) and the frame layout are *frozen* within a `PROTOCOL_MAJOR`: a `[frozen]` line in the golden may be added (with a revision) and never changed, so a relay of any revision from 3 on can route a client of any other. An added line may only pin something no older peer has to read (a new probe, a new fact); a new form of the opening or the hello that a peer sends or must accept needs a new `PROTOCOL_MAJOR`.
- **The security invariants in `DESIGN.md` §11 are requirements:** the fixed remote commands: `fil server --stdio` and the constant bootstrap (decided, owner 2026-11-04; DESIGN §8.2); no secrets at rest; peer-UID checks; pinned certificates. A change that weakens any of them needs the owner's sign-off.

## Delivery

- **Home:** the project's private Gitea; `origin` is its SSH remote. The GitHub repo is a public mirror (below).
- **Workflow:** branch, then PR, then squash merge, through `tea`, which infers the repository from the `origin` remote (`tea pr create --head BRANCH --base main ...`, then `tea pr merge`). One task per PR. Never use `gh`, never contact github.com, and never create a GitHub account: agents have no GitHub access.
- **The merge gate** is two things: `just ci` run locally on the branch, rebased on (or merged with) current `main`, and a green `check` job on the PR's Gitea Actions run (the same `just ci`, on the self-hosted aarch64 runner). It runs `cargo fmt --check`, `clippy -D warnings`, a default-features `cargo check -p fil-shell`, `cargo test` and `cargo deny` (the license, source and ban policy and the RustSec advisories, including yanked crates, in `deny.toml`; the advisory database is fetched, so it needs network access; `FIL_DENY_FLAGS=--offline just ci` checks against the cached copy instead, which can be stale, so an offline pass is not a merge pass). It also fails if the vt100 emulator backend is back in the release binary's dependency graph (`just release-deps`), and if `THIRD-PARTY-NOTICES.md` is stale or a release crate's license is outside the allowed list (`just notices`, which needs cargo-about 0.9.3, the `about_version` in the justfile, and python3; `just notices-update` regenerates the file). Say in the PR that the local run passed.
- **Gitea CI** (`.gitea/workflows/ci.yml`, runner label `orin-aarch64`, aarch64 Linux): `check` runs `just ci`; `musl` builds the static aarch64 and x86_64 `fil` binaries (build only, a smoke test; the release build is still `cross`, `Cross.toml`). It runs on every PR and on pushes to `main`, one job at a time on a runner shared with other projects, so don't re-run in a loop. Jobs have no Docker socket: no `cross`, no realnet, no docker harnesses. A push to `main` whose tree is a PR head's that already passed the same job skips that job's work and reports success (`tests/ci/push_skip.py`, issue #38); release commits and anything in doubt get the full run. Watch runs with `tea actions runs ls`.
- **macOS checks are run by hand** on the dev Mac (`just ci`, the harnesses, `just macos-remote`); there is no macOS runner. Run them when a change touches anything platform-specific on macOS.
- **GitHub is a mirror the owner pushes to by hand**, at release time. The workflows under `.github/workflows/` run there only, at the owner's push (Gitea ignores them, because `.gitea/workflows/` exists), and each event costs GitHub minutes, so keep it to a handful per release. Leave those files alone unless asked. On GitHub, at the owner's push:
  - **Linux check:** `linux-check` runs `just ci` and the on-demand harnesses on Linux. `suite=realnet` runs the real-network harness (real OpenSSH in Docker, NAT and `tc netem` in network namespaces, on static x86_64 musl binaries; `just realnet` on a Linux box, see `docs/m1/realnet.md`); `suite=aarch64` builds the static aarch64 binary for the test Pis (`docs/m1/pi.md`, `just pi-e2e`); `suite=terminals` replays fil's terminal writes (snapshots, the prediction overlay, status lines, resizes) into real xterm, kitty, WezTerm and tmux under Xvfb and reads the screens back (`tests/terminals/run.sh`, `docs/m3/terminals.md`; rerun it whenever `overlay.rs` or the snapshot writer changes a byte it writes); `suite=all` runs `check`, `realnet`, `aarch64` and `terminals`. Agents don't dispatch it; if a change needs it, ask the owner.
  - **macOS check:** `macos-check` (`suite=check|remote|all`) is the same kind of check on GitHub's macOS runners (Apple silicon and Intel): `just ci` and the harnesses on macOS, and macOS as the remote host over a real ssh login (`just macos-remote`, `tests/macos/`).
  - **Release:** `release.yml` builds the release and its dry run (`docs/m4/release.md`).
- **Signing:** the global git config SSH-signs commits. Don't disable it.
- **The repo is public.** What the owner mirrors to GitHub is world-visible, including history, which GitHub keeps. Gitea is private, but everything that reaches `main` or a PR branch may be mirrored, so never commit secrets, IP addresses or personal data, not even in a work-in-progress commit.
- **Reviews and sweeps.**
  - Every PR gets an independent review, and the review always covers **user privacy** (DESIGN.md §11.1) alongside correctness and security.
  - Every milestone ends with a whole-tree **sweep** (DESIGN.md §15): independent audits for redundancy, efficiency, user privacy, security and bugs, then fix PRs, then a verification pass.
  - Privacy here means what fil reveals about the people using it, not just keeping our own secrets out of the repo.
