#!/usr/bin/env python3
"""Unit cases for dep_age.py's decision: `just ci-workflow-test`.

The index is a fake, so the cases run anywhere, offline, in well under a second.
"""

import datetime
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dep_age

NOW = datetime.datetime(2026, 11, 19, 12, 0, 0, tzinfo=datetime.UTC)


def ago(days):
    return (NOW - datetime.timedelta(days=days)).strftime("%Y-%m-%dT%H:%M:%SZ")


INDEX = {
    "old": [
        {"vers": "1.0.0", "pubtime": ago(400)},
        {"vers": "1.0.1", "pubtime": ago(8)},
    ],
    "young": [
        {"vers": "2.0.0", "pubtime": ago(30)},
        {"vers": "2.0.1", "pubtime": ago(2)},
    ],
    "ancient": [
        {"vers": "0.1.0"}
    ],  # no pubtime: published before the index recorded times
}


class Check(unittest.TestCase):
    def run_check(self, crates, allow=None, days=7):
        return dep_age.check(set(crates), INDEX, NOW, days, allow or {})

    def test_old_enough_passes(self):
        self.assertEqual(
            self.run_check([("old", "1.0.0"), ("old", "1.0.1"), ("young", "2.0.0")]), []
        )

    def test_young_version_fails(self):
        problems = self.run_check([("young", "2.0.1")])
        self.assertEqual(len(problems), 1)
        self.assertIn("young 2.0.1", problems[0])
        self.assertIn("2.0 days ago", problems[0])

    def test_boundary_is_inclusive(self):
        self.assertEqual(self.run_check([("old", "1.0.1")], days=8), [])
        self.assertEqual(len(self.run_check([("old", "1.0.1")], days=8.01)), 1)

    def test_missing_pubtime_is_old(self):
        self.assertEqual(self.run_check([("ancient", "0.1.0")]), [])

    def test_unknown_version_fails(self):
        problems = self.run_check([("old", "9.9.9")])
        self.assertEqual(len(problems), 1)
        self.assertIn("not in the crates.io index", problems[0])

    def test_exception_covers_a_young_version(self):
        self.assertEqual(
            self.run_check(
                [("young", "2.0.1")], allow={("young", "2.0.1"): "RUSTSEC-x"}
            ),
            [],
        )

    def test_stale_exception_fails(self):
        problems = self.run_check(
            [("old", "1.0.0")], allow={("old", "1.0.0"): "was urgent"}
        )
        self.assertEqual(len(problems), 1)
        self.assertIn("remove it", problems[0])
        problems = self.run_check([], allow={("gone", "1.0.0"): "was urgent"})
        self.assertEqual(len(problems), 1)


class Inputs(unittest.TestCase):
    def test_index_path(self):
        self.assertEqual(dep_age.index_path("a"), "1/a")
        self.assertEqual(dep_age.index_path("cc"), "2/cc")
        self.assertEqual(dep_age.index_path("nix"), "3/n/nix")
        self.assertEqual(dep_age.index_path("Quinn-Proto"), "qu/in/quinn-proto")

    def write(self, text):
        with tempfile.NamedTemporaryFile("w", suffix=".lock", delete=False) as f:
            f.write(text)
        self.addCleanup(os.unlink, f.name)
        return f.name

    def test_lockfile_skips_path_packages(self):
        path = self.write(
            'version = 4\n[[package]]\nname = "fil"\nversion = "1.0.0"\n'
            '[[package]]\nname = "cc"\nversion = "1.2.0"\n'
            'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
        )
        self.assertEqual(dep_age.locked_crates(path), {("cc", "1.2.0")})

    def test_lockfile_refuses_other_sources(self):
        path = self.write(
            'version = 4\n[[package]]\nname = "x"\nversion = "0.1.0"\nsource = "git+https://example.com/x#abc"\n'
        )
        with self.assertRaises(SystemExit):
            dep_age.locked_crates(path)

    def test_allow_file(self):
        path = self.write("# comment\n\ncc 1.2.0 RUSTSEC-2026-0001: the fix\n")
        self.assertEqual(
            dep_age.read_allow(path), {("cc", "1.2.0"): "RUSTSEC-2026-0001: the fix"}
        )
        with self.assertRaises(SystemExit):
            dep_age.read_allow(self.write("cc 1.2.0\n"))


if __name__ == "__main__":
    unittest.main()
