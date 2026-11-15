#!/usr/bin/env bash
# Lint the release workflow's supply-chain rules (docs/m4/plan.md, REL, "Permissions"). Plain text
# analysis of the workflow's indentation (no YAML library is needed or available on every machine),
# so it relies on the file's block style; selftest.sh mutates the workflow to prove each rule bites.
#
#   lint-workflow.sh [WORKFLOW]      default .github/workflows/release.yml
#   lint-workflow.sh --check WORKFLOW...   the profile of the manual check workflows (linux-check.yml,
#                                    macos-check.yml): rules C1 to C5 at the end of this comment
#
# Rules:
#   1. the workflow's own `permissions:` is `contents: read` and nothing else;
#   2. every job declares `permissions:`; only the job `publish` may have `id-token`, `attestations`
#      or `contents: write`, except that the job `crates-io` (trusted publishing, N8) may have
#      `id-token: write`, and must then run in the `environment: crates-io`, which the owner approves;
#      no other job names an environment;
#   3. `publish` and `crates-io` run only for a push of a `v*` tag: each `if:` is EXACTLY
#      `github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')` (an `||` or any other
#      edit fails); a job's `permissions:` is a block, never `write-all` or an inline { ... };
#   4. every `uses:` is pinned by a full 40-hex commit SHA;
#   5. no secret (the word `secrets` in any case: `Secrets.X`, `secrets ['X']`, `toJSON( secrets )`, `secrets: inherit`), no cache of any kind, no `pull_request_target`;
#   6. every checkout sets `persist-credentials: false`;
#   7. the only triggers are a tag push (no branches) and workflow_dispatch, and workflow_dispatch
#      takes no `inputs:` (an input that reached a step would be attacker-chosen text);
#      each trigger is a block, never inline (`workflow_dispatch: {inputs: ...}` would hide its settings);
#      no `${{ }}` expression inside a `run:` (values reach the shell through `env:`);
#   8. every `cargo`/`cross` build, install, vendor or metadata command, in the workflow and in
#      tests/release/*.sh, passes `--locked`.
#   9. no step runs `just` itself: a recipe runs as `tests/release/just.sh RECIPE`, which reads only
#      the owned tests/release/release.just, never the top-level justfile (it has no code owner, and
#      just evaluates its assignments as soon as it parses it; tests/release/recipes.sh).
#  10. the release scripts' own tests (selftest.sh, which has no code owner) run in a job
#      `release-test` of their own, not beside an owned step, and `publish` needs that job;
#  11. the `prepare` job runs `cargo deny --locked check` with at least `bans` and `sources`, so that
#      deny.toml (owned) bounds the unowned Cargo.lock on the release path itself.
#  12. the digests between the jobs (tests/release/digest.sh; docs/m4/release.md section 9.1):
#      `build-macos` records each attempt's universal binary (outputs sha256_a and sha256_b of its
#      step `digest`); `package` builds the man pages (docs/man/build.sh runs unowned crate code)
#      before it downloads any binary, checks both sets/*/universal-apple-darwin/fil against those
#      outputs before tests/release/package.sh reads them, and records SHA256SUMS (output sums of
#      its step `sums`); `publish` checks the downloaded SHA256SUMS against that output before it
#      attests anything.
#
# The check profile (--check): these workflows run on a dispatch, a pull request or a push of main,
# and hold no secret.
#   C1. the workflow's `permissions:` is a block holding `contents: read` only; no job names a
#       `write` or `write-all` permission (quoted or not), and a job's `permissions:` is a block,
#       `{}` or `read-all` (an inline map such as `{contents: write}` would hide a write);
#   C2. the triggers are workflow_dispatch (required), `pull_request` with no settings (any base branch;
#       no `types:`, `branches:` or `paths:` that would change which events run it; never
#       pull_request_target, whose runs get a write token and the base repository's secrets) and
#       `push` with exactly `branches: [main]` (no tags, paths or other branches); nothing else
#       (workflow_run, issue_comment, schedule, ...). A pull_request run from a fork gets a
#       read-only token and no secret, which C1 and C5 keep true for every run;
#   C3. every `uses:` is pinned by a full 40-hex commit SHA, and every checkout sets
#       `persist-credentials: false`;
#   C4. no `${{ }}` expression inside a `run:` (an input or a matrix value reaches the shell through `env:`);
#   C5. no secret is referenced.
# The scheduled profile (--scheduled): the weekly advisory scan (audit.yml). C1, C3, C4 and C5 as in
# the check profile, and instead of C2:
#   S2. the triggers are exactly `schedule` (required; a block of `- cron:` lines and nothing else)
#       and workflow_dispatch (required) with no settings: no inputs, no pull_request, no push.
# (.gitea/workflows/ci.yml is not covered: it runs on pull requests, so C2 does not apply to it.)
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

if [ "${1:-}" = --check ] || [ "${1:-}" = --scheduled ]; then
  if [ "$1" = --scheduled ]; then sched=1; else sched=0; fi
  profile=$1
  shift
  [ $# -gt 0 ] || { echo "lint-workflow: $profile needs a workflow file" >&2; exit 2; }
  rc=0
  for f in "$@"; do
    [ -f "$f" ] || { echo "lint-workflow: no such file: $f" >&2; exit 2; }
    awk '
      function bad(msg) { print "lint-workflow: " FILENAME ":" NR ": " msg; fails++ }
      function finish_checkout() {
        if (in_checkout && !checkout_ok) bad("checkout (line " checkout_line ") lacks persist-credentials: false")
        in_checkout = 0
      }
      {
        raw = $0
        if (in_run && raw !~ /^[ \t]*$/) {
          match(raw, /^ */)
          if (RLENGTH <= run_indent) in_run = 0
          else if (raw ~ /\$\{\{/) bad("expression inside a run: script (pass it through env:): " raw)
        }
        if (!in_run && raw ~ /^ *(- )?run:/) {
          match(raw, /^ *(- )?/); run_indent = RLENGTH
          val = raw; sub(/^ *(- )?run:[ ]*/, "", val)
          if (val ~ /^[|>]/) in_run = 1
          else if (val ~ /\$\{\{/) bad("expression inside a run: command: " raw)
        }
        line = $0
        sub(/[ \t]+#.*$/, "", line)
        if (line ~ /^[ \t]*#/ || line ~ /^[ \t]*$/) next
        match(line, /^ */); indent = RLENGTH
        if (in_checkout && line ~ /^ *- / && indent <= checkout_indent) finish_checkout()
        if (indent == 0) {
          section = line; sub(/:.*/, "", section)
          if (section == "permissions") { if (line !~ /^permissions:[ ]*$/) bad("top-level permissions must be a block"); top_seen = 1 }
          next
        }
        if (section == "permissions" && indent == 2 && line !~ /^  contents:[ ]*read[ ]*$/) bad("top-level permissions allow more than contents: read: " line)
        if (section == "on" && indent == 2) {
          key = line; sub(/^ */, "", key); sub(/:.*/, "", key)
          trig = key
          if (key == "workflow_dispatch") dispatch_seen = 1
          else if (sched && key == "schedule") schedule_seen = 1
          else if (sched) bad("trigger " key " is not allowed (schedule and workflow_dispatch only)")
          else if (key == "push") push_seen = 1
          else if (key != "pull_request") bad("trigger " key " is not allowed (workflow_dispatch, pull_request and a push of main only)")
          val = line; sub(/^[^:]*:[ ]*/, "", val)
          if (val != "") bad("trigger " key " must be a block, not inline: " line)
        }
        if (sched && section == "on" && indent > 2) {
          if (trig == "schedule" && line ~ /^    - cron:[ ]*["\047][0-9*\/, -]+["\047][ ]*$/) cron_seen = 1
          else if (trig == "schedule") bad("schedule may hold - cron: lines only: " line)
          else bad(trig " takes no settings in the scheduled profile: " line)
        }
        if (!sched && section == "on" && indent > 2) {
          if (trig == "pull_request") bad("pull_request takes no settings (types, branches or paths would change which events run it): " line)
          if (trig == "push") {
            if (indent == 4 && line ~ /^    branches:[ ]*\[[ ]*main[ ]*\][ ]*$/) push_main = 1
            else bad("push may name exactly branches: [main] and nothing else: " line)
          }
        }
        if (section == "jobs" && line ~ /^ *[A-Za-z-]+:[ ]*["\047]?write(-all)?["\047]?[ ]*$/) bad("a write permission: " line)
        # Job permissions are a block (each scope on a line of its own, which the rule above
        # reads) or one of the inline forms that grant nothing more: {} and read-all.
        if (section == "jobs" && line ~ /^ +permissions:[ ]*[^ ]/ && line !~ /^ +permissions:[ ]*(\{[ ]*\}|read-all)[ ]*$/) bad("a job permissions value that is not a block, {} or read-all: " line)
        if (line ~ /write-all/) bad("write-all: " line)
        if (line ~ /uses:/) {
          u = line; sub(/^.*uses:[ ]*/, "", u); gsub(/["]/, "", u)
          at = index(u, "@"); sha = substr(u, at + 1)
          if (at == 0 || length(sha) != 40 || sha !~ /^[0-9a-f]+$/ || u !~ /^[A-Za-z0-9._-]+\/[A-Za-z0-9._\/-]+@/) bad("action not pinned by a 40-hex commit SHA: " u)
          if (u ~ /^actions\/checkout@/) { finish_checkout(); in_checkout = 1; checkout_ok = 0; checkout_line = NR; match(line, /^ *(- )?/); checkout_indent = RLENGTH - (line ~ /^ *- / ? 2 : 0) }
        }
        if (in_checkout && line ~ /persist-credentials:[ ]*false/) checkout_ok = 1
        if (tolower(line) ~ /(^|[^a-z0-9_])secrets([^a-z0-9_]|$)/) bad("a secret is referenced: " line)
      }
      END {
        finish_checkout()
        if (!top_seen) bad("no top-level permissions: contents: read")
        if (!dispatch_seen) bad("no workflow_dispatch trigger")
        if (sched && !cron_seen) bad("no schedule with a cron line")
        if (push_seen && !push_main) bad("push without branches: [main] (it would run on every branch and tag)")
        exit fails ? 1 : 0
      }
    ' sched="$sched" "$f" || rc=1
  done
  exit $rc
fi

wf=${1:-$here/../../.github/workflows/release.yml}
[ -f "$wf" ] || { echo "lint-workflow: no such file: $wf" >&2; exit 2; }
scripts_dir=${FIL_LINT_SCRIPTS:-$here}

awk '
  function bad(msg) { print "lint-workflow: " FILENAME ":" NR ": " msg; fails++ }
  function finish_checkout() {
    if (in_checkout && !checkout_ok) bad("checkout (line " checkout_line ") lacks persist-credentials: false")
    in_checkout = 0
  }
  {
    raw = $0
    # `run:` steps: an expression in the script is text pasted into the shell
    if (in_run && raw !~ /^[ \t]*$/) {
      match(raw, /^ */)
      if (RLENGTH <= run_indent) in_run = 0
      else if (raw ~ /\$\{\{/) bad("expression inside a run: script (pass it through env:): " raw)
    }
    if (!in_run && raw ~ /^ *(- )?run:/) {
      match(raw, /^ *(- )?/); run_indent = RLENGTH
      val = raw; sub(/^ *(- )?run:[ ]*/, "", val)
      if (val ~ /^[|>]/) in_run = 1
      else if (val ~ /\$\{\{/) bad("expression inside a run: command: " raw)
    }
    line = $0
    sub(/[ \t]+#.*$/, "", line)          # trailing comment
    if (line ~ /^[ \t]*#/ || line ~ /^[ \t]*$/) next
    match(line, /^ */); indent = RLENGTH
    # a new step or key at or above the step level ends a checkout step
    if (in_checkout && indent <= checkout_indent && line !~ /^ *with:/ ) {
      if (line ~ /^ *- / || indent < checkout_indent) finish_checkout()
    }
    if (indent == 0) {
      section = line; sub(/:.*/, "", section)
      if (section != "permissions") top_perm_block = 0
      if (section == "permissions") { top_perm_block = 1; if (line !~ /^permissions:[ ]*$/) bad("top-level permissions must be a block") }
      if (section == "jobs") in_jobs = 1
      next
    }
    if (section == "permissions" && top_perm_block && indent == 2) {
      if (line !~ /^  contents:[ ]*read[ ]*$/) bad("top-level permissions allow more than contents: read: " line)
      top_perm_seen = 1
    }
    if (section == "on" && indent == 2) {
      key = line; sub(/^ */, "", key); sub(/:.*/, "", key)
      if (key != "push" && key != "workflow_dispatch") bad("trigger " key " is not allowed (only a tag push and workflow_dispatch)")
      cur_trigger = key
      # a trigger written inline (`workflow_dispatch: {inputs: ...}`, `push: {branches: [main]}`)
      # hides its settings from the block rules below
      val = line; sub(/^[^:]*:[ ]*/, "", val)
      if (val != "") bad("trigger " key " must be a block, not inline: " line)
    }
    if (section == "on" && indent > 2 && cur_trigger == "workflow_dispatch" && line ~ /^ *inputs:/) bad("workflow_dispatch takes inputs: " line)
    if (section == "on" && indent > 2 && cur_trigger == "push") {
      if (line ~ /branches/ || line ~ /paths/) bad("the push trigger may name tags only: " line)
      if (line ~ /tags:/ ) tags_seen = 1
    }
    if (in_jobs && indent == 2 && line ~ /^  [A-Za-z0-9_-]+:[ ]*$/) {
      job = line; sub(/^ */, "", job); sub(/:.*/, "", job)
      jobs[job] = 1; in_perm = 0
      next
    }
    if (in_jobs && job != "") {
      if (indent == 4 && line ~ /^    permissions:/) {
        in_perm = 1; has_perm[job] = 1
        if (line !~ /^    permissions:[ ]*$/) bad("job " job " permissions must be a block, not: " line)
        next
      }
      if (indent == 4) in_perm = 0
      if (in_perm && indent == 6) {
        k = line; sub(/^ */, "", k); v = k; sub(/:.*/, "", k); sub(/^[^:]*:[ ]*/, "", v)
        if (job == "crates-io" && k == "id-token") { if (v != "write") bad("job crates-io has id-token: " v) }
        else if (job != "publish") {
          if (k == "id-token" || k == "attestations") bad("job " job " has " k " (only publish may)")
          if (k == "contents" && v ~ /write/) bad("job " job " has contents: write (only publish may)")
          if (v ~ /write/) bad("job " job " has " k ": write (only publish may)")
        }
      }
      if (indent == 4 && line ~ /^    if:/) job_if[job] = line
      if (indent == 4 && line ~ /^    needs:/) job_needs[job] = line
      if (line ~ /just\.sh release-test/ || line ~ /selftest\.sh/) runs_selftest[job] = 1
      if (job == "prepare" && line ~ /(run:|^)[ ]*cargo deny --locked check( [a-z]+)+[ ]*$/ && line ~ / bans( |$)/ && line ~ / sources( |$)/) deny_seen = 1
      if (indent == 4 && line ~ /^    environment:/) { e = line; sub(/^    environment:[ ]*/, "", e); job_env[job] = e }
      # rule 12: where the digests are recorded and checked (first line of each, per job)
      if (line ~ /actions\/download-artifact@/ && !first_dl[job]) first_dl[job] = NR
      if (line ~ /actions\/attest-build-provenance@/ && !first_attest[job]) first_attest[job] = NR
      if (line ~ /docs\/man\/build\.sh/ && !first_docs[job]) first_docs[job] = NR
      if (line ~ /tests\/release\/package\.sh/ && !first_pkg[job]) first_pkg[job] = NR
      t = line; sub(/^ */, "", t)
      if (t ~ /^sha256_a: \$\{\{ steps\.digest\.outputs\.sha256_a \}\}$/) d12[job, "out_a"] = NR
      if (t ~ /^sha256_b: \$\{\{ steps\.digest\.outputs\.sha256_b \}\}$/) d12[job, "out_b"] = NR
      if (t ~ /^id: digest$/) d12[job, "id_digest"] = NR
      if (t ~ /^run: tests\/release\/digest\.sh out "sha256_\$ATTEMPT" out\/universal-apple-darwin\/fil$/) d12[job, "emit_mac"] = NR
      if (t ~ /^MACOS_SHA256_A: \$\{\{ needs\.build-macos\.outputs\.sha256_a \}\}$/) d12[job, "env_a"] = NR
      if (t ~ /^MACOS_SHA256_B: \$\{\{ needs\.build-macos\.outputs\.sha256_b \}\}$/) d12[job, "env_b"] = NR
      if (t ~ /^tests\/release\/digest\.sh check "\$MACOS_SHA256_A" sets\/a\/universal-apple-darwin\/fil$/ && !d12[job, "check_a"]) d12[job, "check_a"] = NR
      if (t ~ /^tests\/release\/digest\.sh check "\$MACOS_SHA256_B" sets\/b\/universal-apple-darwin\/fil$/ && !d12[job, "check_b"]) d12[job, "check_b"] = NR
      if (t ~ /^sums: \$\{\{ steps\.sums\.outputs\.sums \}\}$/) d12[job, "out_sums"] = NR
      if (t ~ /^id: sums$/) d12[job, "id_sums"] = NR
      if (t ~ /^run: tests\/release\/digest\.sh out sums out\/SHA256SUMS$/) d12[job, "emit_sums"] = NR
      if (t ~ /^SUMS_SHA256: \$\{\{ needs\.package\.outputs\.sums \}\}$/) d12[job, "env_sums"] = NR
      if (t ~ /^tests\/release\/digest\.sh check "\$SUMS_SHA256" out\/SHA256SUMS$/ && !d12[job, "check_sums"]) d12[job, "check_sums"] = NR
    }
    # whole-file rules
    if (line ~ /uses:/) {
      u = line; sub(/^.*uses:[ ]*/, "", u); gsub(/["]/, "", u)
      at = index(u, "@"); sha = substr(u, at + 1)
      if (at == 0 || length(sha) != 40 || sha !~ /^[0-9a-f]+$/ || u !~ /^[A-Za-z0-9._-]+\/[A-Za-z0-9._\/-]+@/) bad("action not pinned by a 40-hex commit SHA: " u)
      if (u ~ /^actions\/checkout@/) { in_checkout = 1; checkout_ok = 0; checkout_line = NR; match(line, /^ *(- )?/); checkout_indent = RLENGTH - (line ~ /^ *- / ? 2 : 0) }
    }
    if (in_checkout && line ~ /persist-credentials:[ ]*false/) checkout_ok = 1
    # the word `secrets` in any case, with or without spaces around it: secrets.X, Secrets.X,
    # SECRETS['X'], secrets ['X'], toJSON( secrets ), format(.., secrets), `secrets: inherit`
    if (tolower(line) ~ /(^|[^a-z0-9_])secrets([^a-z0-9_]|$)/) bad("a secret is referenced: " line)
    if (line ~ /pull_request_target/) bad("pull_request_target: " line)
    if (line ~ /actions\/cache/ || line ~ /rust-cache/ || line ~ /^ *cache:/ || line ~ /[ ]cache-[a-z-]*:/) bad("a cache: " line)
  }
  END {
    tag_if = "    if: github.event_name == '"'"'push'"'"' && startsWith(github.ref, '"'"'refs/tags/v'"'"')"
    finish_checkout()
    if (!top_perm_seen) bad("no top-level permissions: contents: read")
    if (!tags_seen) bad("no tag push trigger")
    for (j in jobs) if (!has_perm[j]) bad("job " j " declares no permissions")
    if (!("publish" in jobs)) bad("no publish job")
    else if (job_if["publish"] != tag_if) bad("publish does not run only for a push of a v* tag: " job_if["publish"])
    if ("crates-io" in jobs) {
      if (job_if["crates-io"] != tag_if) bad("crates-io does not run only for a push of a v* tag: " job_if["crates-io"])
      if (job_env["crates-io"] != "crates-io") bad("crates-io does not run in the environment crates-io: " job_env["crates-io"])
    }
    if (!deny_seen) bad("prepare does not run cargo deny --locked check with bans and sources (the unowned Cargo.lock is bounded by deny.toml there)")
    if (!("release-test" in jobs)) bad("no job release-test (the self-test runs in a job of its own)")
    for (j in runs_selftest) if (j != "release-test") bad("job " j " runs the self-test beside its own steps (only the job release-test may)")
    if (job_needs["publish"] !~ /[[ ,]release-test[],]/) bad("publish does not need release-test: " job_needs["publish"])
    for (j in job_env) if (j != "crates-io") bad("job " j " names an environment (" job_env[j] "); only crates-io may")
    # rule 12
    if (!d12["build-macos", "out_a"] || !d12["build-macos", "out_b"] || !d12["build-macos", "id_digest"] || !d12["build-macos", "emit_mac"])
      bad("build-macos does not record its binary digest (outputs sha256_a and sha256_b from the step id: digest that runs tests/release/digest.sh out)")
    if (!first_docs["package"] || !first_dl["package"] || first_docs["package"] > first_dl["package"])
      bad("package does not build the man pages (docs/man/build.sh, unowned code) before it downloads the binaries")
    if (!d12["package", "env_a"] || !d12["package", "env_b"] || !d12["package", "check_a"] || !d12["package", "check_b"])
      bad("package does not check both macOS binaries against build-macos outputs sha256_a and sha256_b (tests/release/digest.sh check)")
    else if (!first_pkg["package"] || d12["package", "check_a"] > first_pkg["package"] || d12["package", "check_b"] > first_pkg["package"])
      bad("package checks the macOS binaries after tests/release/package.sh reads them, not before")
    if (!d12["package", "out_sums"] || !d12["package", "id_sums"] || !d12["package", "emit_sums"])
      bad("package does not record SHA256SUMS (output sums from the step id: sums that runs tests/release/digest.sh out sums out/SHA256SUMS)")
    if (!d12["publish", "env_sums"] || !d12["publish", "check_sums"])
      bad("publish does not check SHA256SUMS against the package job output sums (tests/release/digest.sh check)")
    else if (!first_attest["publish"] || d12["publish", "check_sums"] > first_attest["publish"])
      bad("publish checks SHA256SUMS after it attests, not before")
    exit fails ? 1 : 0
  }
' "$wf"

# 8. --locked on every build-ish command, in the workflow and in the release scripts
missing=0
check_locked() {
  # a line that runs cargo or cross with a subcommand that resolves dependencies, without --locked
  local f=$1 hits
  hits=$(grep -nE '(^|[^A-Za-z_/-])(cargo|cross) +(build|install|vendor|metadata|fetch|check|test)( |$)' "$f" | grep -v -- '--locked' | grep -vE '^[0-9]+:[ ]*#' || true)
  if [ -n "$hits" ]; then
    echo "lint-workflow: $f: cargo/cross without --locked:" >&2
    echo "$hits" >&2
    missing=1
  fi
}
check_locked "$wf"
for s in "$scripts_dir"/build-binary.sh "$scripts_dir"/macos-build.sh "$scripts_dir"/package.sh "$scripts_dir"/rebuild.sh; do
  [ -f "$s" ] && check_locked "$s"
done
[ "$missing" = 0 ] || exit 1

# 9. no bare `just`: a command line (or a run: value) that starts with it, or runs it after ;, && or |
hits=$(sed 's/[[:space:]]#.*$//' "$wf" | grep -nE '^[[:space:]]*(- )?(run:[[:space:]]*)?just([[:space:]]|$)|(;|&&|[|])[[:space:]]*just([[:space:]]|$)' | grep -vE '^[0-9]+:[[:space:]]*#' || true)
if [ -n "$hits" ]; then
  echo "lint-workflow: $wf: a step runs just, which reads the unowned justfile (use tests/release/just.sh RECIPE):" >&2
  echo "$hits" >&2
  exit 1
fi
echo "lint-workflow: $wf ok"
