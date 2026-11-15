# Security policy

## Reporting a vulnerability

Please report security problems privately, through GitHub's private vulnerability reporting: open <https://github.com/anthonytw/fil/security/advisories/new> (on [github.com/anthonytw/fil](https://github.com/anthonytw/fil), the **Security** tab, then **Report a vulnerability**). Only you and the maintainers see the report. Don't open a public issue or pull request for one.

Say what you found, the fil version (`fil --version`), the platforms and transports involved (the ssh pipe or direct QUIC), and how to reproduce it if you can. Never include real keys, host names or session contents: a synthetic setup is enough. A report about what fil reveals about the people using it (its privacy rules are in [`DESIGN.md`](DESIGN.md) §11.1) is welcome too.

## Supported versions

Only the latest release gets security fixes. A fix ships as a new release; older releases are not patched. Before 1.0 a minor release can change the wire, so a fix can mean upgrading both ends.

## What happens next

This is a volunteer-run project, so these are aims, not promises:

1. **Acknowledge.** You get a reply in the report within a week: whether it reproduces, and its severity (the scale below).
2. **Fix.** The fix is made privately, with a regression test, and you are asked to check it if you wish. Fixes are prioritized by severity: Critical and High in the next release, within 30 days; Medium within 90.
3. **Publish.** When the release with the fix is out, the advisory is published as a GitHub Security Advisory, crediting you unless you'd rather not be named. Whether it gets a CVE is the rule below. The timing is agreed with you; the default is publication with the fixed release.
4. **Tell users.** The release notes and `CHANGELOG.md` say who is affected (which versions, platforms, transports and settings), what could happen, and what to do: upgrade the client and every host, and end the sessions on the old daemon (`fil kill`), since a running daemon keeps its old code until it exits.

## Scope

In scope: getting a shell, a session, a forward or an agent signature without the user's own ssh login; reading or injecting into another user's session or socket on a shared host; anything a network attacker can do to the direct (QUIC) transport; a host using what the user granted (forwards, the agent) beyond what the documents say it can; crashes or resource exhaustion from a hostile peer's input (frames, control messages, the install bootstrap's answers, SOCKS and agent requests, config and policy files); fil revealing more about its users than [`DESIGN.md`](DESIGN.md) §11.1 says; and the release pipeline's integrity (what a download, a package or the installer script can be made to contain).

Out of scope, as [`DESIGN.md`](DESIGN.md) §11 and the [security and privacy](docs/user/security-privacy.md) page say: root on the remote host, a compromised client machine, the terminal emulator's own escape-sequence handling (fil passes bytes through as ssh does), `-R` ports on `127.0.0.1` being reachable by other users of the host by default (as with `ssh -R`; `remote_peers = "same-user"` narrows it), the four documented cases where predictive echo can draw a typed key, and keystroke timing, which fil does not obscure.

## Audits

Besides taking reports, the project audits itself on a schedule and publishes what it found. The runbook is [`docs/audits/`](docs/audits/README.md).

- **A release audit for every minor release** (1.1, 1.2, ...): the changes since the last audited version, reviewed before the release is tagged. A patch release gets one only if it touches security-relevant code.
- **A full audit every six months:** every area of the code on `main`, plus a refresh of the dependencies and the CI pins. When a full audit falls near a release, one report covers both. The first full audit, the baseline, was the security and privacy part of the sweep before 1.0 (2026-11-15). `tests/release/just.sh release` warns (never stops) when the last full audit is more than seven months old.
- **A weekly scan** of the locked dependencies against the RustSec and OSV advisory databases (`.github/workflows/audit.yml`), between audits.
- **Reports are published after the fixes ship**, in [`docs/audits/`](docs/audits/README.md): what was checked, what was found, what was fixed, and which documented claims were checked against the code. Until a fix is in a release, its details stay in a private security advisory, and nothing public (a commit, branch, pull request title or changelog line) describes it. A finding that cannot be fixed within 90 days is published with its details withheld (id, severity, area, advisory link). An audit that finds nothing is still published.
- **The reviewers are AI models** working from the source, with a separate verifier checking each finding (a different agent, and a different model where one is available) and a second reviewer checking the triage. They can miss things. A clean report is evidence, not proof, and it is not a third-party audit or a certification.

Severity, highest first. GitHub advisories use Critical, High, Moderate and Low; Medium here is GitHub's Moderate.

| Rating | Meaning |
| --- | --- |
| Critical | Someone without the user's ssh login gets a shell, a session, a forward or an agent signature, or reads or injects terminal data, remotely (over the network, against the direct transport, or through a release file that passes verification) |
| High | The same, with a precondition an attacker can plausibly get: another unprivileged user on the same host, a hostile host the user connects to reaching the user's machine beyond what they granted, a stolen per-attach key |
| Medium | A documented guarantee fails in a narrower case; something reaches a party the documents say cannot see it (a log line with keystrokes, a host learning a hidden forward target); a peer's input crashes or exhausts the client or another user's daemon |
| Low | Defense in depth, a missing hardening step, a document that overstates, an unlikely local-only leak |
| Info | An observation or a test gap; no action required |

A CVE is requested (through GitHub) for every published advisory rated High or Critical, and for a Medium that breaks a stated guarantee in a released version. Low, Info, hardening and findings only in unreleased code get none.

| Audit | Date | Version | Report |
| --- | --- | --- | --- |
| Full (the baseline) | 2026-11-15 | 1.0.0 | [`docs/audits/2026-11-15-full-1.0.0.md`](docs/audits/2026-11-15-full-1.0.0.md) |

## Verifying releases

Every file of a release is in its `SHA256SUMS`, which the owner signs with the release key (`SHA256SUMS.sig`) and the package key (`SHA256SUMS.asc`). How to check a download, and the keys' fingerprints, are in the [install guide](docs/user/install.md#checking-a-download) and the [README](README.md); the installer script (`fil-install.sh`) does the same checks before it installs anything.
