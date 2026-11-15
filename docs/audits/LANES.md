# Audit lanes

One lane is one reviewer with one brief. A lane reads the code, the design and the claims for its area, and returns findings in the schema below. The runbook is [`README.md`](README.md); change a brief by PR. Briefs name files and symbols, not line numbers, which move.

## Rules for every lane

- Read-only: change nothing in the repository. Write findings, and any reproduction, in the run's working folder, never in the repository.
- Check the previous reports first: did each earlier finding in your area stay fixed, and does it have a regression test?
- Rate by the documented guarantees (`DESIGN.md` §11 and §11.1, `SECURITY.md`, `docs/user/security-privacy.md`, the lane's documents), not by taste. A rating is an argument about a sentence: state which documented guarantee or hard rule (`CLAUDE.md`, "The security invariants") breaks, or "none: new risk".
- Out of scope per `SECURITY.md` "Scope" (root on the remote host, a compromised client machine, the terminal emulator's own escape handling, the documented predictive-echo residuals, keystroke timing): do not report it as a finding; list it under "Not a finding" with the reason.
- Never put a real key, host name, address, user name or session content in a finding. Use synthetic hosts (`*.example`, documentation addresses) and throwaway keys.
- If you cannot check a claim, say "cannot check" and why.

## Finding schema

One JSON object per finding; a lane writes an array to `findings/<lane>.json`. The verifier adds `verdict` and `verifier_notes`; the coordinator writes the report's table from these files.

| Field | Content |
| --- | --- |
| `id` | lane letter and number, `A-03` |
| `lane` | `A` to `G` |
| `title` | one line, neutral wording (no exploit detail in anything that may become public) |
| `severity` | Critical, High, Medium, Low or Info (`SECURITY.md` "Audits") |
| `confidence` | high, medium or low |
| `component` | `path:line` (and symbol) |
| `claim` | the documented guarantee or hard rule it breaks, or "none: new risk" |
| `attack` | who can do what, with what access (a network attacker, another user on the host, a hostile host, a hostile peer program) |
| `evidence` | a reproduction or a test name; for a read-only observation, the code path |
| `fix` | a proposal |
| `status` | open, fixed in PR or release, accepted risk, or not confirmed |

## A. Wire and parsers

**Covers:** `fil-proto` (the preamble, the hello and the opening ping and pong, frames, control messages, the mux, input and text encodings, the limits in `limits.rs`), the golden encoding (`crates/fil-proto/tests/golden/wire.txt`) and its frozen lines, the terminal emulator's input (`fil-term`: what a hostile program's output can make the snapshot writer, the sniffers and the OSC capture do), the install bootstrap's answer parser (`fil-transport/src/pipe/answer.rs`, `fil/src/install/probe.rs`), and every fuzz target (`crates/*/fuzz/`).
**Starts from:** `DESIGN.md` §8.5 (frames and channels), §6 (the terminal stream), `CLAUDE.md` "The wire has a revision".
**Questions:** does every decoder enforce the limit `limits.rs` names before it allocates? Can a peer of another revision, or a hostile one, get past the hello, or reach a decoder the hello should have refused? Does a frozen line still mean what it says? Does hostile input exhaust memory, time or stack (lengths, counts, nesting) or panic? Do the fuzz targets reach every decoder? Run each fuzz target for a fixed time (`cargo fuzz run TARGET -- -max_total_time=N`, nightly) and record the durations and any crash.

## B. Transport, auth and keys

**Covers:** the ssh pipe (`fil-transport/src/pipe/`: the command built for ssh, what reaches the remote shell), direct QUIC (`fil-transport/src/quic/`: the listener, the rate limiter, the inbox, the floor), per-attach keys and their registry (`fil-transport/src/auth/`: the identity, the registry, the verification, revocation on detach, takeover and session end), certificate pinning and SNI, the client's dialer and consent (`fil/src/{connect,dialer,link}.rs`, `fil-client/src/{dialer,consent}.rs`), and fd limits.
**Starts from:** `DESIGN.md` §8 and §11 (the table), `docs/m1/quic-auth.md`, `docs/m1/quic-link.md`, `docs/user/transports.md`.
**Questions:** can anyone without the registered key complete a QUIC handshake, resume a session or get an answer that tells a scanner fil is there? Is a key usable after revocation, or for another session? Is any certificate trusted on first use? Does any secret (a key, a token) reach a log, a file, an argument list or the environment? Is the remote command ever built from user input (`remote-commands.txt` is the fixed list)? Can a flood exhaust the daemon before authentication?

## C. Daemon, sessions, policy, bootstrap and upload

**Covers:** the daemon's start, its socket directory and peer-UID checks (`fil-daemon/src/{start,paths,hardening,conn,daemon}.rs`), the relay, sessions (`fil-session`: the PTY, takeover and epochs, the environment scrub in `env.rs`, the private directories, the session trampoline), the server policy (`fil-session/src/policy/`: `policy.toml`, the sshd mirror and its patterns), the management commands (`fil/src/mgmt.rs`, `server.rs`), and the install path: the constant bootstrap, the upload with its size and SHA-256 check, the layout and the garbage collection (`fil/src/install/`).
**Starts from:** `DESIGN.md` §5, §8.2, §11 and §11.2, `docs/m4/install.md`, `docs/m4/release.md` §5 and §6, `docs/user/policy.md`, `docs/user/daemon.md`.
**Questions:** can another user on the host reach a socket, a session, a log or an install directory, by a race, a symbolic link or a directory they made first? Does the bootstrap run anything before the upload's size and digest are checked, and does every directory from `$HOME` down get checked? Can the policy be bypassed (a pattern, an sshd setting fil does not mirror, an unclear case that is allowed instead of refused)? Does takeover or a stale epoch ever hand a session to the wrong client without asking? Does the environment scrub miss a variable that describes the login?

## D. Forwarding, SOCKS, agent

**Covers:** `-L`, `-R` and `-D` forwards and their persistence across reconnects (`fil-forward`, `fil-session/src/forward.rs`, `fil-client/src/forward.rs`, `fil/src/forward_spec.rs`), the SOCKS parser (`fil-forward/src/socks.rs`), the opaque tags for targets beyond loopback, `remote_peers = "same-user"`, pacing and its limits, `fil ports` and its process attribution (`fil-daemon/src/ports/`), and agent forwarding (`fil-forward/src/agent/`: the filter, the relay; `fil-session/src/agent.rs`, `fil-client/src/agent.rs`).
**Starts from:** `DESIGN.md` §7, §10 and §11, `docs/user/forwards.md`, `docs/user/agent.md`.
**Questions:** can the host learn a `-R` target or a `-L`/`-D` bind on the laptop's network that the design says stays hidden? Can a forward reach a destination the user did not ask for, or outlive its owner? Does `same-user` check every connection before a byte reaches the laptop? Does the agent filter allow anything but list, sign and the two extensions, sign for a key the agent did not list, or exist while no client asked for it? Is a destination-constrained key refused rather than bypassed? Does `fil ports` name another user or their processes?

## E. Prediction secrets

**Covers:** predictive echo (`fil-predict`: the predictor, the tracker, the overlay and its epochs, the key classes), the client's engine and its repair path (`fil-client/src/engine.rs`, `engine/`), the daemon's echo acknowledgments (`fil-session/src/echo.rs`), and what a guess can write to the terminal.
**Starts from:** `DESIGN.md` §9 and its row in the §11 table, `docs/m3/plan.md` §4.4, the "Predictive echo" section of the guide.
**Questions:** is anything predicted at a canonical, echo-off prompt, or outside a confirmed epoch? Do the four accepted residual cases still describe everything that can draw a typed key, and is any of them wider than documented? Does a guess, a key or a count reach a log above the documented level? Does `predict = "off"` turn every guess off?

## F. Supply chain and release

**Covers:** `Cargo.lock` and `deny.toml`, the notices (`about.toml`, `about.hbs`), the toolchain and image pins (`rust-toolchain.toml`, `Cross.toml`, the images in `tests/release/`), the GitHub Actions pins, permissions and triggers, the release scripts (`tests/release/`), the release files and their signatures (`SHA256SUMS`, `.sig`, `.asc`), the attestations, the installer script (`packaging/install.sh`) and its pinned key, the apt and dnf repositories (`tests/release/repos.sh`), the Homebrew formula and AUR templates (`packaging/`), the Nix flake, the crates.io publishing job, CODEOWNERS, and what the Gitea mirror receives.
**Starts from:** `docs/m4/release.md`, `docs/m4/packaging.md`, `.github/CODEOWNERS`.
**Questions:** run `just deny` and `osv-scanner --lockfile Cargo.lock`; refresh the Action pins; confirm the weekly workflow is enabled. Does any release step run something it downloaded, or accept an input that no signature or rebuild covers? Does the installer refuse every verification failure and never run unverified content? Do the installer's pinned key and `.github/release-signers` agree? Does the apt or dnf repository's metadata bind each package by digest, signed by the package key? Does any workflow get a write token or a secret it does not need, or run on a trigger an outsider controls? Does CODEOWNERS still cover everything that decides what a release is?

## G. Privacy and claims ledger

**Covers:** everything fil sends, records, keeps or shows about its users: every connection fil opens (the hosts the user named, through their pipe; nothing else), every log line (`logging.rs` in `fil`, `fil-daemon`; the levels; files 0600 and directories 0700; rotation), the offline-input log, what the remote session's environment carries, what `fil ls`, takeover messages and `fil ports` show to whom, what fil writes on the host and on the laptop. Then one pass over every sentence of `SECURITY.md`, `DESIGN.md` §11 and §11.1 and `docs/user/security-privacy.md`: each claim gets a verdict.
**Starts from:** `DESIGN.md` §11.1, `docs/user/security-privacy.md`.
**Verdicts:** holds with test X; holds, no test (becomes a test task in the report); does not hold; cannot check.
**Questions:** is "nothing leaves your machines" true of every code path, the installer and the packages included (they download only from the release URL the user ran)? Does any log at any level carry terminal output, keystrokes, command lines, environment variables or offline input? Does fil leave anything on the host beyond the three documented directories? The ledger is the cheapest way to catch drift: the documents are public promises.
