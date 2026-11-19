#!/usr/bin/env python3
"""Refuse locked crates.io versions younger than a minimum age (`just dep-age`, part of `just lint`).

A hijacked maintainer account publishes a malicious version, and it lives on crates.io until
someone notices and yanks it: the arrayref, internment and append-only-vec versions of
2026-08-20 lasted about 90 minutes. A lockfile that picks up such a version in that window builds
its build script or proc macro on every machine that runs `cargo build`. Waiting is the cheap
defence: a version that has been public for two weeks has been looked at, scanned and, if it was bad,
yanked (which `cargo deny` refuses). So no lockfile here may hold a crates.io version published
less than `--days` days (default 14) ago. The check runs on every PR (`just lint`) and on the
release candidate, so a release never ships a dependency younger than that.

The publish time is the `pubtime` field of the crates.io sparse index (index.crates.io, a static
CDN; one request per crate, not per version). crates.io stamps it at publish time (a publisher
cannot backdate it) and has filled it in for every older version too, so a version without one is
a problem. Anything that cannot be fetched or read fails the check: it never passes silently.
Needs Python 3.11 or later (tomllib).

An urgent fix (a security release we must take now) goes in tests/ci/dep-age-allow.txt as
`NAME VERSION REASON`, with a reason, and comes out once it is old enough; a stale entry fails.
"""

import argparse
import concurrent.futures
import datetime
import http.client
import json
import os
import subprocess
import sys
import time
import tomllib
import urllib.error
import urllib.request

CRATES_IO = {
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
}
INDEX = "https://index.crates.io/"
USER_AGENT = "fil-ci-dep-age"
HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
DEFAULT_ALLOW = os.path.join(HERE, "dep-age-allow.txt")


def index_path(name):
    """The sparse-index path of a crate (the cargo registry index layout)."""
    n = name.lower()
    if len(n) == 1:
        return f"1/{n}"
    if len(n) == 2:
        return f"2/{n}"
    if len(n) == 3:
        return f"3/{n[0]}/{n}"
    return f"{n[0:2]}/{n[2:4]}/{n}"


def locked_crates(lockfile):
    """The (name, version) pairs a lockfile takes from crates.io."""
    with open(lockfile, "rb") as f:
        lock = tomllib.load(f)
    out = set()
    for p in lock.get("package", []):
        src = p.get("source")
        if src is None:
            continue  # a path (workspace) package
        if src not in CRATES_IO:
            raise SystemExit(
                f"dep-age: {lockfile}: {p['name']} {p['version']} comes from {src}, not crates.io"
            )
        out.add((p["name"], p["version"]))
    return out


def fetch_index(name, tries=3):
    """The index entries of a crate, one dict per published version."""
    url = INDEX + index_path(name)
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    for attempt in range(tries):
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                body = r.read().decode()
            return [json.loads(line) for line in body.splitlines() if line.strip()]
        except (
            urllib.error.URLError,
            http.client.HTTPException,
            TimeoutError,
            OSError,
            ValueError,
        ) as e:
            if attempt == tries - 1:
                raise SystemExit(f"dep-age: cannot fetch {url}: {e}")
            time.sleep(2 * (attempt + 1))
    raise AssertionError("unreachable")


def parse_time(s):
    return datetime.datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ").replace(
        tzinfo=datetime.UTC
    )


def read_allow(path):
    """{(name, version): reason} from the exceptions file."""
    allow = {}
    if not os.path.exists(path):
        return allow
    with open(path) as f:
        for n, line in enumerate(f, 1):
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            parts = line.split(None, 2)
            if len(parts) < 3:
                raise SystemExit(f"dep-age: {path}:{n}: want NAME VERSION REASON")
            allow[(parts[0], parts[1])] = parts[2]
    return allow


def check(crates, index, now, days, allow):
    """The problems, as lines: too young, unknown to the index, or a stale exception."""
    min_age = datetime.timedelta(days=days)
    problems = []
    used = set()
    for name, version in sorted(crates):
        entry = next((e for e in index[name] if e.get("vers") == version), None)
        if entry is None:
            problems.append(f"{name} {version}: not in the crates.io index")
            continue
        pubtime = entry.get("pubtime")
        if pubtime is None:
            problems.append(
                f"{name} {version}: the crates.io index gives no publish time"
            )
            continue
        age = now - parse_time(pubtime)
        if age >= min_age:
            continue
        if (name, version) in allow:
            used.add((name, version))
            continue
        hours = age.total_seconds() / 3600
        problems.append(
            f"{name} {version}: published {pubtime} ({hours / 24:.1f} days ago), younger than {days} days"
        )
    for name, version in sorted(set(allow) - used):
        problems.append(
            f"{name} {version}: in the exceptions file but not needed (not locked, or old enough now): remove it"
        )
    return problems


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument(
        "lockfiles", nargs="*", help="default: every Cargo.lock in the repository"
    )
    ap.add_argument("--days", type=float, default=14.0)
    ap.add_argument("--allow", default=DEFAULT_ALLOW)
    args = ap.parse_args(argv)

    lockfiles = args.lockfiles or [
        os.path.join(ROOT, f)
        for f in subprocess.run(
            ["git", "-C", ROOT, "ls-files", "--", "Cargo.lock", "*/Cargo.lock"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.split()
    ]
    crates = set()
    for lf in lockfiles:
        crates |= locked_crates(lf)
    names = sorted({n for n, _ in crates})
    with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
        index = dict(zip(names, pool.map(fetch_index, names)))
    now = datetime.datetime.now(datetime.UTC)
    problems = check(crates, index, now, args.days, read_allow(args.allow))
    if problems:
        print(
            f"dep-age: {len(problems)} problem(s) in {len(lockfiles)} lockfile(s):",
            file=sys.stderr,
        )
        for p in problems:
            print(f"  {p}", file=sys.stderr)
        print(
            f"dep-age: keep the older version (`cargo update -p NAME --precise OLD`), or, for an urgent fix, "
            f"add `NAME VERSION REASON` to {os.path.relpath(args.allow, ROOT)}",
            file=sys.stderr,
        )
        return 1
    print(
        f"dep-age: {len(crates)} crates.io versions in {len(lockfiles)} lockfile(s), all at least {args.days:g} days old"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
