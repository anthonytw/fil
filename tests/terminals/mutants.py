#!/usr/bin/env python3
"""Mutations of the exported traces, for checking that the suite notices what it should
(docs/m3/terminals.md, "Mutation checks").

    mutants.py NAME TRACE_DIR OUT_DIR

writes into OUT_DIR the traces of TRACE_DIR that the mutation touches, with their bytes changed
and their expectations left alone, as if the overlay had written those bytes. Run them with
`run.py TERMINAL OUT_DIR RESULTS` and the traces that notice show up as disagreements (use
TERMINALS_STRICT=1 so that known.tsv does not hide them). The mutations:

  no-origin-restore  the overlay forgets `CSI ? 6 h` after a paint or an erase (the program's own is kept)
  wrong-erase        the erase writes `#` where it should restore the cell
  no-pen-reset       the paint leaves its underline pen on (drops the `CSI 0 m` after the guess)
"""

import json
import os
import re
import sys

ERASE = re.compile(rb"(\x1b\[\?25l\x1b\[\d+;\d+H) (\x1b\[\d+;\d+H)")
ORIGIN = re.compile(rb"(\x1b\[0;4m[^\x1b]|\x1b\[\d+;\d+H )\x1b\[\?6h")
PAINT = re.compile(rb"(\x1b\[0;4m[^\x1b])\x1b\[0m")


def mutate(name, data):
    if name == "no-origin-restore":
        new, n = ORIGIN.subn(rb"\1", data)
        return new if n else None
    if name == "wrong-erase":
        new, n = ERASE.subn(rb"\1#\2", data)
        return new if n else None
    if name == "no-pen-reset":
        new, n = PAINT.subn(rb"\1", data)
        return new if n else None
    raise SystemExit(f"unknown mutation {name}")


def main():
    name, src, dst = sys.argv[1:4]
    os.makedirs(dst, exist_ok=True)
    n = 0
    for fn in sorted(os.listdir(src)):
        if not fn.endswith(".json") or not fn.startswith(("ov-", "st-")):
            continue
        t = json.load(open(os.path.join(src, fn)))
        touched = False
        for step in t["steps"]:
            if step["op"] == "write":
                new = mutate(name, bytes.fromhex(step["hex"]))
                if new is not None and new != bytes.fromhex(step["hex"]):
                    step["hex"] = new.hex()
                    touched = True
        if touched:
            json.dump(t, open(os.path.join(dst, fn), "w"))
            n += 1
    print(f"{name}: {n} traces mutated in {dst}")


if __name__ == "__main__":
    main()
