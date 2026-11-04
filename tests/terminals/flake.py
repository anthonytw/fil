#!/usr/bin/env python3
"""Does a trace flake because of the read-backs between its flushes? (docs/m3/terminals.md, finding 8)

    flake.py TERMINAL TRACE_DIR WORK_DIR RUNS TRACE...

Runs each named trace RUNS times in three forms, with no retries and no known.tsv, and counts
the distinct outcomes (the hashes of the differences at each checkpoint):

  as-is      the trace as exported: a read-back after every flush, the flushes written separately
  no-readback  the same writes, one after the other, with only the last checkpoint kept
  one-write  every write joined into one, with only the last checkpoint kept

FLAKE_FORMS="as-is no-readback" limits the forms. Needs Xvfb for the GUI terminals: run it under `xvfb-run -a`.
"""

import collections
import hashlib
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


def variants(src, dst, names):
    for form in ("as-is", "no-readback", "one-write"):
        d = os.path.join(dst, form)
        os.makedirs(d, exist_ok=True)
        for n in names:
            t = json.load(open(os.path.join(src, n + ".json")))
            steps = t["steps"]
            if form != "as-is":
                last = [s for s in steps if s["op"] == "check"][-1]
                writes = [s for s in steps if s["op"] == "write"]
                if form == "one-write":
                    writes = [{"op": "write", "hex": "".join(w["hex"] for w in writes)}]
                steps = writes + [last]
            t["steps"] = steps
            json.dump(t, open(os.path.join(d, n + ".json"), "w"))


def main():
    term, src, work, runs = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
    names = sys.argv[5:]
    variants(src, work, names)
    env = dict(os.environ, TERMINALS_STRICT="1", TERMINALS_NO_RETRY="1")
    for form in os.environ.get("FLAKE_FORMS", "as-is no-readback one-write").split():
        outcomes = collections.defaultdict(collections.Counter)
        for i in range(runs):
            out = os.path.join(work, "res")
            subprocess.run(
                [sys.executable, "-I", os.path.join(HERE, "run.py"), term, os.path.join(work, form), out],
                env=env, capture_output=True,
            )
            per = collections.defaultdict(list)
            for line in open(os.path.join(out, f"{term}.jsonl")):
                r = json.loads(line)
                if "check" in r:
                    per[r["trace"]].append(
                        r["check"] + ":" + (hashlib.sha256("\n".join(r["diffs"]).encode()).hexdigest()[:6] if r["diffs"] else "ok")
                    )
            for tr, v in per.items():
                outcomes[tr][" ".join(v)] += 1
        print(f"== {form}: {runs} runs")
        for tr in names:
            print(f"  {tr}: {len(outcomes[tr])} distinct outcome(s)")
            for o, c in outcomes[tr].most_common():
                print(f"    {c:>3}x {o}")


if __name__ == "__main__":
    main()
