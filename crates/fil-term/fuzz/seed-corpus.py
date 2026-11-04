#!/usr/bin/env python3
"""Rebuilds seeds/emulator from the recorded fixtures (../tests/fixtures/*.raw and *.meta).

Each seed is `cols rows history resize_cols resize_rows split` and then the recording (see
fuzz_targets/emulator.rs): the fixture's own size, a resize narrower and one wider, the
recording cut in the middle. The target takes the columns modulo 199 plus two and the rows modulo 80 plus one.
"""
import pathlib
import shutil

here = pathlib.Path(__file__).resolve().parent
fixtures = here.parent / "tests" / "fixtures"
out = here / "seeds" / "emulator"
if out.exists():
    shutil.rmtree(out)
out.mkdir(parents=True)

for raw in sorted(fixtures.glob("*.raw")):
    meta = dict(
        (k.strip(), int(v))
        for k, v in (l.split("=", 1) for l in raw.with_suffix(".meta").read_text().splitlines() if "=" in l and l.split("=", 1)[1].strip().isdigit())
    )
    ncols, nrows = meta["cols"], meta["rows"]
    cols, rows = ncols - 2, nrows - 1  # the target adds two columns and one row
    data = raw.read_bytes()
    for tag, (rc, rr, split) in {
        "same": (cols, rows, 255),
        "narrow": (max(ncols * 2 // 3, 4) - 2, rows, 128),
        "wide": (ncols * 3 // 2 - 2, rows, 64),
    }.items():
        (out / f"{raw.stem}-{tag}").write_bytes(bytes([cols, rows, 250, rc % 256, rr, split]) + data)
print(len(list(out.iterdir())), "seeds")
