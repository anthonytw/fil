#!/usr/bin/env python3
"""Summarizes the results of tests/terminals/run.sh as Markdown tables.

    report.py OUT_DIR

OUT_DIR holds one `TERMINAL.jsonl` per terminal that ran. The tables are what
docs/m3/terminals.md quotes.
"""

import json
import os
import sys
from collections import OrderedDict

ORDER = ["xterm", "kitty", "wezterm", "tmux"]


def load(out):
    res = OrderedDict()
    for t in ORDER:
        path = os.path.join(out, f"{t}.jsonl")
        if os.path.exists(path):
            res[t] = [json.loads(x) for x in open(path) if x.strip()]
    return res


def main():
    out = sys.argv[1]
    res = load(out)
    terms = list(res)
    # Checks per terminal by status.
    print("| terminal | checks | agree | disagree | informational | errors |")
    print("|---|---|---|---|---|---|")
    for t, recs in res.items():
        c = [r for r in recs if "check" in r and r["trace"] != "canary"]
        ok = sum(r["status"] == "ok" for r in c)
        dif = sum(r["status"] == "diff" for r in c)
        inf = sum(r["status"] == "info" for r in c)
        err = sum("error" in r for r in recs)
        print(f"| {t} | {len(c)} | {ok} | {dif} | {inf} | {err} |")
    print()

    # Disagreements by trace.
    traces = OrderedDict()
    for t, recs in res.items():
        for r in recs:
            if "check" in r:
                d = traces.setdefault(r["trace"], {}).setdefault(t, [0, 0])
                d[1] += 1
                d[0] += r["status"] == "diff"
    print("| trace | " + " | ".join(terms) + " |")
    print("|---|" + "---|" * len(terms))
    for tr, per in traces.items():
        if not any(v[0] for v in per.values()):
            continue
        cells = []
        for t in terms:
            v = per.get(t)
            cells.append("" if v is None else ("ok" if v[0] == 0 else f"{v[0]}/{v[1]}"))
        print(f"| {tr} | " + " | ".join(cells) + " |")
    print()

    # Widths: columns the cursor advanced.
    print("| string | unicode-width | model | " + " | ".join(terms) + " |")
    print("|---|---|---|" + "---|" * len(terms))
    rows = OrderedDict()
    for t, recs in res.items():
        for r in recs:
            if r.get("trace") == "wd-widths" and "check" in r:
                row = rows.setdefault(r["check"], {"uw": (r.get("refs") or {}).get("unicode_width"), "model": None})
                exp = (r.get("expected") or {}).get("cursor")
                row["model"] = exp[1] - 1 if exp else None
                obs = (r.get("observed") or {}).get("cursor")
                row[t] = obs[1] - 1 if obs else None
    for name, row in rows.items():
        vals = [row.get(t) for t in terms]
        flag = "" if all(v == row["model"] for v in vals) and row["uw"] == row["model"] else " *"
        print(f"| {name}{flag} | {row['uw']} | {row['model']} | " + " | ".join(str(v) for v in vals) + " |")
    print()

    # Scrollback after a clear.
    print("| trace | check | model | " + " | ".join(terms) + " |")
    print("|---|---|---|" + "---|" * len(terms))
    rows = OrderedDict()
    for t, recs in res.items():
        for r in recs:
            if r.get("trace", "").startswith("cl-") and "check" in r:
                row = rows.setdefault((r["trace"], r["check"]), {})
                row["model"] = (r.get("expected") or {}).get("history_count")
                row[t] = (r.get("observed") or {}).get("history_count")
    for (tr, ck), row in rows.items():
        print(f"| {tr} | {ck} | {row['model']} | " + " | ".join(str(row.get(t)) for t in terms) + " |")


if __name__ == "__main__":
    main()
