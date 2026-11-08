#!/usr/bin/env python3
"""Decide whether a push-to-main run of a Gitea CI job may skip its work (issue #38).

    push_skip.py            run by .gitea/workflows/ci.yml in the job's checkout

A squash merge whose PR branch already had the latest main merged in produces a commit whose tree
is byte for byte the PR head's tree, and the PR head's run of the same job already passed on that
tree. Running it again on main costs the shared runner up to half an hour and proves nothing new.
The tree holds the workflow itself, so the earlier run ran the very steps this one would.

The job may skip only if ALL of these hold; anything else, including any error, means a full run:

1. the event is a push to refs/heads/main and the job token is set;
2. HEAD is the pushed commit and it is not a release commit (below);
3. the commit subject ends in `(#N)`, Gitea's squash-merge subject;
4. `refs/pull/N/head` fetches, and its tree is HEAD's tree;
5. the Gitea API's combined status of that head commit (which holds the latest status of each
   context) has this job's pull-request context (`<workflow> / <job> (pull_request)`, e.g.
   `ci / check (pull_request)`), it is `success`, and its target_url is a run of this
   repository's Actions (`/<owner>/<repo>/actions/runs/<id>/...`), not a status someone posted
   through the API with another link.

The PR number only says where to look: the guarantee is (4) and (5), the same tree passed the same
job, whichever commit carried it. Only the commit-status endpoint is used (repository code read,
which the job token has); git fetches use the same token, as the checkout step does.

The target_url check does not authenticate the status: a repo writer could post a success with an
Actions-looking link. That is no weaker than the PR merge gate, which reads the same latest status
of the same context, and the skipped tree's own workflow would have run on the push anyway.

A PR run can be old, so a skip still runs `just deny` (the workflow's job): an advisory published
since the PR run still fails main. Only the build and test work is skipped.

Release commits always get a full run, so a release whose tag is on its bump commit is tested on
main itself (a tag on a later commit is only as tested as that commit's push run; release.yml
rebuilds a tag from scratch either way). A release commit is one that changes the `version = ` line of the root Cargo.toml (the
`[workspace.package]` version: the only line of that file starting so), or a `## [` release
heading of CHANGELOG.md, or whose subject starts with `Release `. The workflow cannot know at push
time that a tag will follow, and those are what a release commit changes (docs/m4/release.md), so
they are cheap to read from `git diff HEAD^ HEAD` and err only towards running. If HEAD has no
parent that can be fetched, that counts as a release commit too.

Environment: GITHUB_EVENT_NAME, GITHUB_REF, GITHUB_SHA, GITHUB_SERVER_URL, GITHUB_REPOSITORY,
FIL_CI_TOKEN (the job token), FIL_CI_CONTEXT (the PR status context to look for), GITHUB_OUTPUT,
GITHUB_STEP_SUMMARY (optional). Writes `skip=true` or `skip=false` and `reason=...` to
GITHUB_OUTPUT, logs why (and, on a skip, the same line to the job summary), and exits 0 either way:
a crash of this script leaves the output unset, which the workflow also reads as a full run.
"""

import json
import os
import re
import subprocess
import sys
import urllib.request

SUBJECT_PR = re.compile(r"\(#([0-9]+)\)\s*$")
RELEASE_LINE = re.compile(r"^[+-](version = |## \[)")


class Skip(Exception):
    """Not a reason to fail: the decision is a full run, for the reason given."""


def is_release(subject, diff):
    """True if the commit is (or might be) a release commit. `diff` is `git diff -U0 HEAD^ HEAD --
    Cargo.toml CHANGELOG.md`, or None when there is no parent to diff against."""
    if diff is None:
        return "its parent could not be fetched, so a release cannot be ruled out"
    if subject.startswith("Release "):
        return "the subject starts with 'Release '"
    for line in diff.splitlines():
        if line.startswith(("+++", "---")):
            continue
        if RELEASE_LINE.match(line):
            return f"it changes a release line: {line.strip()}"
    return None


def decide(env, git, api):
    """Return (skip, reason). `git(args) -> str` runs git and returns stdout, raising on failure;
    `api(path) -> object` GETs /api/v1/repos/<repo>/<path> and returns the decoded JSON, raising on
    failure. Every check failing or raising leads to (False, reason)."""
    try:
        return _decide(env, git, api)
    except Skip as e:
        return False, str(e)
    except Exception as e:  # noqa: BLE001 - any doubt is a full run
        return False, f"error ({type(e).__name__}: {e})"


def _decide(env, git, api):
    if env.get("GITHUB_EVENT_NAME") != "push" or env.get("GITHUB_REF") != "refs/heads/main":
        raise Skip("not a push to main")
    if not env.get("FIL_CI_TOKEN"):
        raise Skip("no job token")
    context = env.get("FIL_CI_CONTEXT", "")
    if not context.endswith(" (pull_request)"):
        raise Skip(f"bad FIL_CI_CONTEXT {context!r}")
    sha = env.get("GITHUB_SHA", "")
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise Skip(f"bad GITHUB_SHA {sha!r}")
    if git(["rev-parse", "HEAD"]).strip() != sha:
        raise Skip("HEAD is not the pushed commit")

    subject = git(["log", "-1", "--format=%s", "HEAD"]).strip()
    try:
        git(["fetch", "-q", "--deepen=1", "origin", sha])
        diff = git(["diff", "-U0", "HEAD^", "HEAD", "--", "Cargo.toml", "CHANGELOG.md"])
    except Exception:  # noqa: BLE001
        diff = None
    why = is_release(subject, diff)
    if why:
        raise Skip(f"release commit: {why}")

    m = SUBJECT_PR.search(subject)
    if not m:
        raise Skip(f"the subject names no PR: {subject!r}")
    pr = m.group(1)

    git(["fetch", "-q", "--depth=1", "origin", f"refs/pull/{pr}/head"])
    head = git(["rev-parse", "FETCH_HEAD"]).strip()
    head_tree = git(["rev-parse", "FETCH_HEAD^{tree}"]).strip()
    tree = git(["rev-parse", "HEAD^{tree}"]).strip()
    if head_tree != tree:
        raise Skip(f"tree {tree} differs from PR #{pr} head {head}'s tree {head_tree}")

    status = api(f"commits/{head}/status")
    entry = next((s for s in status.get("statuses") or [] if s.get("context") == context), None)
    if entry is None:
        raise Skip(f"PR #{pr} head {head} has no '{context}' status")
    state = entry.get("status")
    if state != "success":
        raise Skip(f"PR #{pr} head {head}: '{context}' is {state!r}, not success")
    url = entry.get("target_url") or ""
    run = re.fullmatch(
        rf"/{re.escape(env.get('GITHUB_REPOSITORY', ''))}/actions/runs/([0-9]+)(/.*)?", url
    )
    if not env.get("GITHUB_REPOSITORY") or not run:
        raise Skip(f"PR #{pr} head {head}: '{context}' links {url!r}, not this repo's Actions run")
    return True, f"tree identical to PR #{pr} head {head}, run {run.group(1)} ({url})"


def main():
    env = os.environ

    # The token reaches git through its environment, not its argv: a failed command's message
    # (logged as the reason) quotes the argv.
    git_env = dict(env)
    git_env.update(
        GIT_CONFIG_COUNT="1",
        GIT_CONFIG_KEY_0="http.extraHeader",
        GIT_CONFIG_VALUE_0=f"Authorization: token {env.get('FIL_CI_TOKEN', '')}",
    )

    def git(args):
        return subprocess.run(
            ["git", *args], check=True, capture_output=True, text=True, timeout=120, env=git_env
        ).stdout

    def api(path):
        url = f"{env['GITHUB_SERVER_URL']}/api/v1/repos/{env['GITHUB_REPOSITORY']}/{path}"
        req = urllib.request.Request(url, headers={"Authorization": f"token {env['FIL_CI_TOKEN']}"})
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.load(r)

    skip, reason = decide(env, git, api)
    line = f"skipped: {reason}" if skip else f"full run: {reason}"
    line = line.replace("\n", " ")
    print(line)
    summary = env.get("GITHUB_STEP_SUMMARY")
    if skip and summary:
        with open(summary, "a") as f:
            f.write(line + "\n")
    out = env.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a") as f:
            f.write(f"reason={line}\nskip={'true' if skip else 'false'}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
