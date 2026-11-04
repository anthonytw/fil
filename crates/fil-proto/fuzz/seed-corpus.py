#!/usr/bin/env python3
"""Rebuilds seeds/ from the wire golden (../tests/golden/wire.txt).

Run it from anywhere after `just wire-golden` if you want the seeds to follow a new revision;
stale seeds are harmless, so it is not part of any gate. Each target gets the golden's bytes
for its input, in the shape the target reads (see the doc comment of each fuzz_targets/*.rs):
the pieced targets (preamble, frame, mux) start with the piece-size byte, and preamble and frame
with a limit byte too.
"""
import pathlib
import shutil

here = pathlib.Path(__file__).resolve().parent
golden = here.parent / "tests" / "golden" / "wire.txt"
out = here / "seeds"

lines = {}
for raw in golden.read_text().splitlines():
    if raw.startswith("#") or " = " not in raw:
        continue
    k, v = raw.split(" = ", 1)
    section = k.split("/", 1)[0]
    lines.setdefault(section, []).append((k, v))


def hexes(section):
    """(name, bytes) for the lines of `section` whose value is hex."""
    for k, v in lines.get(section, []):
        try:
            yield k.replace("/", "-"), bytes.fromhex(v)
        except ValueError:
            pass


seeds = {}


def add(target, name, data):
    seeds.setdefault(target, {})[name] = data


# preamble: [piece size][limit][bytes]. 0xff takes it whole, 1 a byte at a time; limit 0 is the default.
for n, b in hexes("preamble"):
    add("preamble", n, bytes([0xFF, 0]) + b)
    add("preamble", n + "-bytewise", bytes([1, 0]) + b)
    add("preamble", n + "-after-noise", bytes([7, 0]) + b"motd: welcome\n" + b + b"rest")
    add("preamble", n + "-small-limit", bytes([0xFF, 9]) + b"0123456789" + b)

# frame: [piece size][body limit][frame bytes]. Limit 0 is the default.
for n, b in hexes("frame"):
    add("frame", n, bytes([0xFF, 0]) + b)
whole = b"".join(b for _, b in hexes("frame") if len(b) < 200)
add("frame", "all-bytewise", bytes([1, 0]) + whole)
add("frame", "all-by-5", bytes([5, 0]) + whole)
add("frame", "all-small-limit", bytes([0xFF, 1]) + whole)

# control: the payload as it is.
for n, b in hexes("control"):
    add("control", n, b)
for n, b in hexes("hello"):
    add("control", n, b)
for n, b in hexes("error"):
    add("control", n, b)

# opening: a payload.
for k in ("opening",):
    for n, b in hexes(k):
        add("opening", n, b)
for n, b in hexes("hello"):
    add("opening", n, b)
add("opening", "control-hello", dict(hexes("control"))["control-hello"])

# input: a terminal-channel payload (a data segment or a record).
for n, b in hexes("input"):
    add("input", n, b)
for n, b in hexes("data"):
    add("input", n, b)

# mux: each golden line is a whole record (length varint, then the body). The target reads a
# piece-size byte first, then the stream; and it hands the whole input to the body decoder too.
for n, b in hexes("mux"):
    add("mux", n, bytes([0xFF]) + b)
    # the body alone (the first byte stands in for the piece size)
    i = 1 if b[0] < 0x80 else 2
    add("mux", n + "-body", b[i:])
recs = b"".join(b for n, b in hexes("mux") if len(b) < 60)
add("mux", "many-bytewise", bytes([1]) + recs)
add("mux", "many-by-3", bytes([3]) + recs)

if out.exists():
    shutil.rmtree(out)
for target, files in seeds.items():
    d = out / target
    d.mkdir(parents=True)
    for name, data in files.items():
        (d / name).write_bytes(data)
    print(f"{target}: {len(files)} seeds, {sum(map(len, files.values()))} bytes")
