#!/usr/bin/env python3
"""Unit cases for push_skip.py's decision (issue #38): `just ci-workflow-test`.

git and the Gitea API are fakes, so the cases run anywhere, offline, in well under a second.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import push_skip

SHA = "a" * 40
HEAD = "b" * 40
TREE = "c" * 40
OTHER_TREE = "d" * 40
CONTEXT = "ci / check (pull_request)"

ENV = {
    "GITHUB_EVENT_NAME": "push",
    "GITHUB_REF": "refs/heads/main",
    "GITHUB_SHA": SHA,
    "FIL_CI_TOKEN": "t",
    "FIL_CI_CONTEXT": CONTEXT,
    "GITHUB_REPOSITORY": "o/r",
}
RUN_URL = "/o/r/actions/runs/129/jobs/298"

PLAIN_DIFF = """diff --git a/Cargo.toml b/Cargo.toml
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -40 +40 @@
-tokio = { version = "1.47", features = ["rt"] }
+tokio = { version = "1.48", features = ["rt"] }
"""


class FakeGit:
    def __init__(self, subject="Fix a thing (#41)", diff="", head_tree=TREE, fail=()):
        self.subject, self.diff, self.head_tree, self.fail = subject, diff, head_tree, fail
        self.calls = []

    def __call__(self, args):
        self.calls.append(args)
        for f in self.fail:
            if f in args:
                raise RuntimeError(f"git {' '.join(args)} failed")
        a = tuple(args)
        if a == ("rev-parse", "HEAD"):
            return SHA + "\n"
        if a[:2] == ("log", "-1"):
            return self.subject + "\n"
        if a[0] == "fetch":
            return ""
        if a[0] == "diff":
            return self.diff
        if a == ("rev-parse", "FETCH_HEAD"):
            return HEAD + "\n"
        if a == ("rev-parse", "FETCH_HEAD^{tree}"):
            return self.head_tree + "\n"
        if a == ("rev-parse", "HEAD^{tree}"):
            return TREE + "\n"
        raise AssertionError(f"unexpected git {args}")


def api_with(*statuses, fail=False, url=RUN_URL):
    def api(path):
        assert path == f"commits/{HEAD}/status", path
        if fail:
            raise OSError("HTTP Error 401: Unauthorized")
        return {
            "state": "x",
            "statuses": [{"context": c, "status": s, "target_url": url} for c, s in statuses],
        }

    return api


GREEN = api_with((CONTEXT, "success"), ("ci / musl (pull_request)", "failure"))


class Decide(unittest.TestCase):
    def run_case(self, env=ENV, git=None, api=GREEN):
        return push_skip.decide(env, git or FakeGit(), api)

    def assert_full(self, result, why):
        skip, reason = result
        self.assertFalse(skip, reason)
        self.assertIn(why, reason)

    def test_green_match_skips(self):
        skip, reason = self.run_case()
        self.assertTrue(skip, reason)
        self.assertIn(f"tree identical to PR #41 head {HEAD}, run 129", reason)

    def test_dependency_version_bump_is_not_a_release(self):
        self.assertTrue(self.run_case(git=FakeGit(diff=PLAIN_DIFF))[0])

    def test_red_match_runs(self):
        self.assert_full(self.run_case(api=api_with((CONTEXT, "failure"))), "'failure'")

    def test_pending_match_runs(self):
        self.assert_full(self.run_case(api=api_with((CONTEXT, "pending"))), "'pending'")

    def test_only_other_jobs_green_runs(self):
        api = api_with(("ci / check (push)", "success"), ("ci / musl (pull_request)", "success"))
        self.assert_full(self.run_case(api=api), "no 'ci / check (pull_request)' status")

    def test_status_not_from_this_repos_actions_runs(self):
        for url in ("https://ci.example/run/1", "/o/other/actions/runs/1/jobs/2", "", None):
            api = api_with((CONTEXT, "success"), url=url)
            self.assert_full(self.run_case(api=api), "not this repo's Actions run")

    def test_missing_repository_runs(self):
        env = {k: v for k, v in ENV.items() if k != "GITHUB_REPOSITORY"}
        self.assert_full(self.run_case(env=env), "not this repo's Actions run")

    def test_tree_mismatch_runs(self):
        self.assert_full(self.run_case(git=FakeGit(head_tree=OTHER_TREE)), "differs")

    def test_no_pr_in_subject_runs(self):
        self.assert_full(self.run_case(git=FakeGit(subject="Hand-pushed fix")), "names no PR")

    def test_pr_ref_fetch_failure_runs(self):
        self.assert_full(self.run_case(git=FakeGit(fail=("refs/pull/41/head",))), "error")

    def test_api_failure_runs(self):
        self.assert_full(self.run_case(api=api_with(fail=True)), "401")

    def test_api_garbage_runs(self):
        self.assert_full(self.run_case(api=lambda path: ["not", "a", "dict"]), "error")

    def test_missing_token_runs(self):
        env = dict(ENV, FIL_CI_TOKEN="")
        git = FakeGit()
        self.assert_full(self.run_case(env=env, git=git), "no job token")
        self.assertEqual(git.calls, [])

    def test_pull_request_event_runs(self):
        self.assert_full(
            self.run_case(env=dict(ENV, GITHUB_EVENT_NAME="pull_request")), "not a push"
        )

    def test_other_branch_runs(self):
        self.assert_full(self.run_case(env=dict(ENV, GITHUB_REF="refs/heads/x")), "not a push")

    def test_head_not_pushed_commit_runs(self):
        self.assert_full(self.run_case(env=dict(ENV, GITHUB_SHA="e" * 40)), "not the pushed commit")

    def test_release_version_bump_runs(self):
        diff = '--- a/Cargo.toml\n+++ b/Cargo.toml\n@@ -6 +6 @@\n-version = "0.1.0"\n+version = "0.2.0"\n'
        git = FakeGit(diff=diff)
        self.assert_full(self.run_case(git=git), "release commit")
        self.assertFalse(any("refs/pull/41/head" in c for c in git.calls))

    def test_release_changelog_heading_runs(self):
        diff = "--- a/CHANGELOG.md\n+++ b/CHANGELOG.md\n@@ -5 +5 @@\n-## [Unreleased]\n+## [0.2.0] - 2026-12-01\n"
        self.assert_full(self.run_case(git=FakeGit(diff=diff)), "release commit")

    def test_release_subject_runs(self):
        self.assert_full(
            self.run_case(git=FakeGit(subject="Release 0.2.0 (#50)")), "release commit"
        )

    def test_unfetchable_parent_counts_as_release(self):
        self.assert_full(self.run_case(git=FakeGit(fail=("--deepen=1",))), "release commit")


if __name__ == "__main__":
    unittest.main(verbosity=1)
