#!/usr/bin/env bash
# Lint the release workflow's supply-chain rules (docs/m4/plan.md, REL, "Permissions"). Plain text
# analysis of the workflow's indentation (no YAML library is needed or available on every machine),
# so it relies on the file's block style; selftest.sh mutates the workflow to prove each rule bites.
#
#   lint-workflow.sh [WORKFLOW]      default .github/workflows/release.yml
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
#   5. no secret (`secrets.X`, `secrets['X']`, `toJSON(secrets)`, `secrets: inherit`), no cache of any kind, no `pull_request_target`;
#   6. every checkout sets `persist-credentials: false`;
#   7. the only triggers are a tag push (no branches) and workflow_dispatch, and workflow_dispatch
#      takes no `inputs:` (an input that reached a step would be attacker-chosen text);
#      each trigger is a block, never inline (`workflow_dispatch: {inputs: ...}` would hide its settings);
#      no `${{ }}` expression inside a `run:` (values reach the shell through `env:`);
#   8. every `cargo`/`cross` build, install, vendor or metadata command, in the workflow and in
#      tests/release/*.sh, passes `--locked`.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
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
      if (indent == 4 && line ~ /^    environment:/) { e = line; sub(/^    environment:[ ]*/, "", e); job_env[job] = e }
    }
    # whole-file rules
    if (line ~ /uses:/) {
      u = line; sub(/^.*uses:[ ]*/, "", u); gsub(/["]/, "", u)
      at = index(u, "@"); sha = substr(u, at + 1)
      if (at == 0 || length(sha) != 40 || sha !~ /^[0-9a-f]+$/ || u !~ /^[A-Za-z0-9._-]+\/[A-Za-z0-9._\/-]+@/) bad("action not pinned by a 40-hex commit SHA: " u)
      if (u ~ /^actions\/checkout@/) { in_checkout = 1; checkout_ok = 0; checkout_line = NR; match(line, /^ *(- )?/); checkout_indent = RLENGTH - (line ~ /^ *- / ? 2 : 0) }
    }
    if (in_checkout && line ~ /persist-credentials:[ ]*false/) checkout_ok = 1
    # secrets by any spelling: secrets.X, secrets['X'], toJSON(secrets), `secrets: inherit`
    if (line ~ /secrets\./ || line ~ /secrets\[/ || line ~ /\(secrets\)/ || line ~ /^ *secrets:/) bad("a secret is referenced: " line)
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
    for (j in job_env) if (j != "crates-io") bad("job " j " names an environment (" job_env[j] "); only crates-io may")
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
echo "lint-workflow: $wf ok"
