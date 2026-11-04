#!/usr/bin/env python3
"""`just notices`' scan of the crates' own sources (docs/m4/release.md section 7).

    notice_scan.py RENDERED METADATA_JSON

RENDERED is cargo-about's output (its "Crates" table names the crates of the release graph) and
METADATA_JSON is `cargo metadata --format-version 1` (for the crates' directories). It does two
things cargo-about does not:

1. Prints, on stdout, the Markdown for "Copyright notices in source files": every distinct
   `Copyright` line of every file carrying the ISC permission notice, in each crate of the graph
   whose license expression has ISC in it. ISC asks for "the above copyright notice" in all copies,
   and those crates (ring, rustls-webpki, untrusted) hold files by other authors than the one in
   their LICENSE. The recipe puts the output in place of @SOURCE_NOTICES@, so a new holder makes the
   committed file stale.
2. Fails (exit 1) if a crate of the graph has a NOTICE file (Apache-2.0 section 4(d)) whose text
   is not in RENDERED.

The input is the checksum-pinned crates of Cargo.lock, so the output does not depend on the machine.
"""

import json
import os
import re
import sys

ISC = re.compile(
    r"Permission\s+to\s+use,\s+copy,\s+modify,\s+and(?:/or)?\s+distribute\s+this\s+software\s+for\s+any\s+purpose\s+with\s+or\s+without\s+fee",
    re.IGNORECASE,
)
COMMENT = re.compile(r"^\s*(?:/\*+!?|\*+(?!/)|//+[!/]?|#+|;+|@+)?\s*")


def crates_of(rendered):
    rows, on = [], False
    for line in rendered.splitlines():
        if line == "| Crate | Version | License |":
            on = True
            continue
        if on:
            if not line.startswith("|"):
                break
            cells = [c.strip() for c in line.strip("|").split("|")]
            if len(cells) == 3 and cells[0] != "---":
                rows.append(tuple(cells))
    return rows


def holders(path):
    try:
        with open(path, encoding="utf-8") as f:
            text = f.read()
    except (UnicodeDecodeError, OSError):
        return []
    lines = [COMMENT.sub("", l).rstrip() for l in text.replace("\r", "").splitlines()[:200]]
    lines = [re.sub(r"\s*\*+/\s*$", "", l) for l in lines]
    m = ISC.search(" ".join(lines))
    if not m:
        return []
    # The header before the permission notice, as the (stripped) lines it was joined from.
    head, n = [], 0
    for l in lines:
        n += len(l) + 1
        head.append(l)
        if n > m.start():
            break
    out = []
    for l in head:
        l = " ".join(l.split())
        if re.match(r"(?:portions\s+)?copyright\b", l, re.IGNORECASE) and "notice" not in l.lower():
            out.append(l)
    return out


def main():
    rendered_path, meta_path = sys.argv[1], sys.argv[2]
    with open(rendered_path, encoding="utf-8") as f:
        rendered = f.read()
    with open(meta_path, encoding="utf-8") as f:
        meta = json.load(f)
    dirs = {(p["name"], p["version"]): os.path.dirname(p["manifest_path"]) for p in meta["packages"]}
    crates = crates_of(rendered)
    if not crates:
        sys.exit("notice_scan: no Crates table in the render")
    fails = []
    blocks = []
    for name, version, lic in crates:
        d = dirs.get((name, version))
        if d is None:
            fails.append(f"{name} {version}: not in cargo metadata")
            continue
        # NOTICE files (Apache-2.0 section 4(d)): their text must be reproduced.
        for entry in sorted(os.listdir(d)):
            if entry.upper().startswith("NOTICE") and os.path.isfile(os.path.join(d, entry)):
                with open(os.path.join(d, entry), encoding="utf-8", errors="replace") as f:
                    body = f.read().replace("\r", "").strip()
                if body and body not in rendered:
                    fails.append(f"{name} {version} ships {entry}, and its text is not in the notices")
        if not re.search(r"\bISC\b", lic):
            continue
        found = set()
        for root, subdirs, files in os.walk(d):
            subdirs.sort()
            for fn in files:
                found.update(holders(os.path.join(root, fn)))
        if found:
            blocks.append((name, version, sorted(found)))
    if fails:
        for f in fails:
            print(f"notice_scan: {f}", file=sys.stderr)
        sys.exit(1)
    out = []
    for name, version, lines in blocks:
        out.append(f"{name} {version}:")
        out.append("")
        out.append("```text")
        out.extend(lines)
        out.append("```")
        out.append("")
    sys.stdout.write("\n".join(out).rstrip("\n") + "\n")


if __name__ == "__main__":
    main()
