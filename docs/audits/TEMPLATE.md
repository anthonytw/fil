# Security and privacy audit: <version> (<kind: full | release | full + release>)

<!-- Copy to docs/audits/YYYY-MM-DD-KIND-VERSION.md (README.md, "Publishing the report"). -->

| | |
| --- | --- |
| Run | <YYYY-MM-DD> (pinned <date, ET>, published <date, ET>) |
| Code audited | <tag or SHA>; from <last audited tag> (release audits) |
| Method | AI reviewers in lanes, independent verification (see "Method"); not a third-party audit |
| Result | <N Critical, N High, N Medium, N Low, N Info>; <N fixed, N open> |
| Advisories | <GHSA links, or none> |

## Summary

Three to six sentences: what was checked, what mattered, what changed. Say plainly if nothing was found. A clean report is evidence, not proof.

## Scope

The lanes run (A to G) and their paths; for a release audit, the diff and the path-to-lane result (`docs/audits/README.md`, "Path to lane").

### Out of scope, and why

Each excluded area, with the reason (not reachable, needs real hardware or a real network, a third-party service, not yet built). Also the "Not a finding" list: documented non-goals that were considered and left alone (`SECURITY.md`, "Scope").

## Method

Models per lane and for verification and triage review; the commit pinned; tools and versions (rustc, cargo-deny, osv-scanner, the fuzz targets with their durations); what was run (tests, fuzzing, harnesses, `just deny`, the OSV scan with its date) and what was only read. Limits: what an AI source review cannot show.

## Findings

| ID | Severity | Lane | Title | Component | Status | Fixed in |
| --- | --- | --- | --- | --- | --- | --- |

One subsection per finding: the claim it breaks, the attack (who, with what access), the evidence, the fix, who is affected (versions, platforms, transports, settings) and what users should do. A withheld finding shows the id, severity, area and advisory link only. Each severity carries one line of reasoning.

## Not confirmed

Candidate findings the verifier could not confirm, with the reason.

## Claims ledger

| Claim (document and sentence) | Verdict | Evidence (test or `file:line`) |
| --- | --- | --- |

Verdicts: holds with test X; holds, no test (name the test added or the follow-up); does not hold; cannot check.

## Privacy review

What leaves the user's machines (each connection, with the `DESIGN.md` §11.1 line that allows it); logging, per level; telemetry (none); what fil writes on the laptop and on the host, against `docs/user/security-privacy.md`; what other clients and other users on the host can see.

## Dependencies and release integrity

The locked crates with advisories found (`just deny`, the OSV scan and its date); the pins (`Cargo.lock`, `rust-toolchain.toml`, `Cross.toml` and the other images, the Actions); the last release's checksums, signatures and attestations (`tests/release/just.sh release-verify X.Y.Z --no-rebuild`), the installer's pinned key, the apt and dnf repositories' signatures, summarized; CODEOWNERS coverage.

## Follow-ups

Tests to add, documents to change and items for the next run, measured against the previous report: is each earlier finding still fixed, and does it have a regression test?

## Reports from outside the audit

Advisories received since the last run, and what became of each.
