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
- **Tests:** keep them proportionate. Unit-test the logic that is easy to get wrong, such as frame parsing, sequence numbers and the snapshot writer. Heavy harnesses, like the fidelity suite and randomized overlay runs, are commands you run on demand, not part of `just ci` (GitHub's `suite=check` runs a few of them on every PR, below). A test whose pass or fail depends on real elapsed time is named `wallclock_*` and runs alone in `just test-wallclock`, which is not in `just ci` (shared CI runners stall) but in `just ci-release`, run before a release; run it locally too when a change touches what such a test measures (pacing, forwards, latency) (`docs/dev/timing-tests.md`: P and T bounds, `fil_testkit::timing::Stalls`); a bound that is a DESIGN.md claim is never relaxed to make CI pass.
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

- **Home:** the project's private Gitea; `origin` is its SSH remote, and PRs and merges happen there. The GitHub repo (`github` remote) is a public mirror of releases and runs the CI (below); moving PRs back to GitHub comes with the CI rework after 0.2.0.
- **Workflow:** branch, then PR, then squash merge, through `tea`, which infers the repository from the `origin` remote (`tea pr create --head BRANCH --base main ...`, then `tea pr merge`). One task per PR. Agents may use `gh` and push `ci/BRANCH` refs to the `github` remote for the checks below, but keep GitHub activity low: one dispatch of each check per PR head, polling at 60 s or slower, no loops of reruns, no other GitHub writes. Never push GitHub `main` or tags (the owner does, at release time), and never create a GitHub account.
- **The merge gate** (since 2026-11-09) is three things, on the branch rebased on (or merged with) current `main`:
  1. **Local, on the Mac:** the tests for what the change touches (`cargo test -p CRATE FILTER`, the affected harness or `just release-test` for release tooling) plus `just fmt-check clippy deny`. The full `just ci` is not required locally; run it when a change is broad. Say in the PR what ran.
  2. **GitHub:** a green `linux-check` and `macos-check` (`suite=check`) on the PR head, dispatched on a `ci/BRANCH` ref (`gh workflow run linux-check.yml --ref ci/BRANCH -f ref=SHA -f suite=check`, the same for `macos-check.yml`). Both run `just ci` (the justfile's `ci:` line: `cargo deny` with the fetched RustSec database, `just notices`, `ci-workflow-test`, `exporter-lock`, fmt, clippy, the default-features check, `just release-deps`, `just release-test` and `cargo test`, without the wall-clock tests) plus some harnesses (the workflow files list them), on x86_64 Linux and on Apple silicon and Intel macOS. Not per-PR any more: the static musl builds and the aarch64 Linux tests (the Orin jobs); `suite=realnet`, `suite=aarch64` and the release build cover the builds, and `just vm-run just ci` (the Mac's aarch64 Linux VM) or `just pi-e2e` the aarch64 tests; use them when a change touches the build or platform code. Delete the `ci/BRANCH` ref after the merge.
  3. **An independent review**, privacy included.
  The heavy suites (`suite=realnet`, `aarch64`, `terminals`, `latency`, `all`, `remote`), the prediction matrix (`just predict-suites`) and the wall-clock tests (`just ci-release`) run before a release, or when a change is in their area; they are not per-PR.
- **Gitea CI** (`.gitea/workflows/ci.yml`, the self-hosted aarch64 runner) is manual only since 2026-11-09 (`workflow_dispatch`): it is no longer a merge gate. Its push-to-main skip (`tests/ci/push_skip.py`, issue #38) stays for the day it runs on pushes again.
- **GitHub's `main` and tags are pushed by the owner by hand**, at release time. The workflows under `.github/workflows/` run there only (Gitea ignores them, because `.gitea/workflows/` exists); they are listed in CODEOWNERS, so the owner merges changes to them. On GitHub:
  - **Linux check:** `linux-check` runs `just ci` and the on-demand harnesses on Linux. `suite=realnet` runs the real-network harness (real OpenSSH in Docker, NAT and `tc netem` in network namespaces, on static x86_64 musl binaries; `just realnet` on a Linux box, see `docs/m1/realnet.md`); `suite=aarch64` builds the static aarch64 binary for the test Pis (`docs/m1/pi.md`, `just pi-e2e`); `suite=terminals` replays fil's terminal writes (snapshots, the prediction overlay, status lines, resizes) into real xterm, kitty, WezTerm and tmux under Xvfb and reads the screens back (`tests/terminals/run.sh`, `docs/m3/terminals.md`; rerun it whenever `overlay.rs` or the snapshot writer changes a byte it writes); `suite=all` runs `check`, `realnet`, `aarch64` and `terminals`. Agents dispatch `suite=check` per PR (the gate) and the other suites only when a change is in their area.
  - **macOS check:** `macos-check` (`suite=check|remote|all`) is the same kind of check on GitHub's macOS runners (Apple silicon and Intel): `just ci` and the harnesses on macOS, and macOS as the remote host over a real ssh login (`just macos-remote`, `tests/macos/`).
  - **Release:** `release.yml` builds the release and its dry run (`docs/m4/release.md`).
- **Signing:** the global git config SSH-signs commits. Don't disable it.
- **The repo is public.** What the owner mirrors to GitHub is world-visible, including history, which GitHub keeps. Gitea is private, but everything that reaches `main` or a PR branch may be mirrored, so never commit secrets, IP addresses or personal data, not even in a work-in-progress commit.
- **Reviews and sweeps.**
  - Every PR gets an independent review, and the review always covers **user privacy** (DESIGN.md §11.1) alongside correctness and security.
  - Every milestone ends with a whole-tree **sweep** (DESIGN.md §15): independent audits for redundancy, efficiency, user privacy, security and bugs, then fix PRs, then a verification pass.
  - Privacy here means what fil reveals about the people using it, not just keeping our own secrets out of the repo.
