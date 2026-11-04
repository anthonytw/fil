"""Read a .deb or .rpm without running anything from it (release-check, docs/m4/packaging.md).

    python3 -I pkg_inspect.py PACKAGE OUTDIR

Writes the package's files under OUTDIR (each at its installed path, without the leading `/`), and
prints, one per line:

    name <name>
    version <version>          (a .deb's Version, or an rpm's VERSION-RELEASE)
    arch <architecture>
    script <what>              for every maintainer script or trigger the package has
    depends <what>             for every dependency (an rpm's `rpmlib(...)` features are not ones)
    signed yes|no              an rpm: whether its signature header holds an OpenPGP signature
                               (a .deb: always no; dpkg verifies no signature inside a .deb)
    body <sha256>              an rpm: the digest of everything after its signature header (the
                               header and the payload), which signing leaves alone; a .deb: of the file
    file <mode in octal> <owner>:<group> <path>      for every regular file
    dir <mode in octal> <owner>:<group> <path>       for every directory the package lists

The parsers take only what release-check needs: a .deb is an ar archive of debian-binary,
control.tar.gz and data.tar.gz; an rpm is a lead, a signature header, a header and a gzip-compressed
cpio (newc) payload. Anything else (another compression, a link, a device, a path outside OUTDIR) is
an error, which fails the check: linux-packages.sh makes neither.
"""

import gzip
import hashlib
import io
import os
import struct
import sys
import tarfile


def fail(msg: str) -> None:
    sys.exit(f"pkg_inspect: {msg}")


def safe_join(out: str, path: str) -> tuple:
    rel = path[2:] if path.startswith("./") else path.lstrip("/")
    parts = [p for p in rel.split("/") if p]
    if not parts or any(p in (".", "..") for p in parts):
        fail(f"unsafe path in the package: {path!r}")
    return os.path.join(out, *parts), "/" + "/".join(parts)


def write_file(out: str, path: str, data: bytes) -> str:
    dest, shown = safe_join(out, path)
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    with open(dest, "wb") as f:
        f.write(data)
    return shown


# ---------------------------------------------------------------- .deb
def ar_members(blob: bytes) -> dict:
    if not blob.startswith(b"!<arch>\n"):
        fail("not an ar archive")
    pos, members = 8, {}
    while pos < len(blob):
        hdr = blob[pos : pos + 60]
        if len(hdr) < 60 or hdr[58:60] != b"`\n":
            fail("a malformed ar header")
        name = hdr[0:16].decode("ascii").strip().rstrip("/")
        size = int(hdr[48:58].decode("ascii").strip())
        members[name] = blob[pos + 60 : pos + 60 + size]
        pos += 60 + size + (size & 1)
    return members


def deb(path: str, out: str) -> None:
    with open(path, "rb") as f:
        m = ar_members(f.read())
    if m.get("debian-binary") != b"2.0\n":
        fail("debian-binary is not 2.0")
    if "control.tar.gz" not in m or "data.tar.gz" not in m:
        fail(f"expected control.tar.gz and data.tar.gz, found {sorted(m)}")
    with tarfile.open(fileobj=io.BytesIO(m["control.tar.gz"]), mode="r:gz") as t:
        for name in t.getnames():
            if name not in ("./control", "./md5sums", "."):
                print(f"script {name}")
        control = t.extractfile("./control") or fail("no control file")
        fields = {}
        for line in control.read().decode("utf-8").splitlines():
            if line and not line[0].isspace() and ":" in line:
                k, v = line.split(":", 1)
                fields[k] = v.strip()
    print(f"name {fields.get('Package', '')}")
    print(f"version {fields.get('Version', '')}")
    print(f"arch {fields.get('Architecture', '')}")
    for k in ("Depends", "Pre-Depends", "Recommends", "Suggests", "Enhances", "Breaks", "Conflicts", "Replaces", "Provides"):
        if k in fields:
            print(f"depends {k}: {fields[k]}")
    extra = sorted(set(m) - {"debian-binary", "control.tar.gz", "data.tar.gz"})
    for name in extra:
        print(f"script ar member {name}")
    print("signed no")
    with open(path, "rb") as f:
        print(f"body {hashlib.sha256(f.read()).hexdigest()}")
    with tarfile.open(fileobj=io.BytesIO(m["data.tar.gz"]), mode="r:gz") as t:
        for e in t.getmembers():
            owner = f"{e.uname or e.uid}:{e.gname or e.gid}"
            if e.uid != 0 or e.gid != 0:
                owner = f"{e.uid}:{e.gid}"
            if e.isdir():
                _, shown = safe_join(out, e.name) if e.name not in (".", "./") else (None, None)
                if shown:
                    print(f"dir {e.mode:o} {owner} {shown}")
            elif e.isreg():
                shown = write_file(out, e.name, t.extractfile(e).read())
                print(f"file {e.mode:o} {owner} {shown}")
            else:
                fail(f"{e.name}: neither a file nor a directory")


# ---------------------------------------------------------------- .rpm
def rpm_header(blob: bytes, pos: int):
    if blob[pos : pos + 3] != b"\x8e\xad\xe8":
        fail("a malformed rpm header")
    nindex, hsize = struct.unpack(">II", blob[pos + 8 : pos + 16])
    index = blob[pos + 16 : pos + 16 + 16 * nindex]
    store = blob[pos + 16 + 16 * nindex : pos + 16 + 16 * nindex + hsize]
    tags = {}
    for i in range(nindex):
        tag, typ, off, count = struct.unpack(">IIII", index[16 * i : 16 * i + 16])
        if typ in (6, 9):  # STRING, I18NSTRING
            tags[tag] = store[off:].split(b"\0", 1)[0].decode("utf-8")
        elif typ == 8:  # STRING_ARRAY
            tags[tag] = [s.decode("utf-8") for s in store[off:].split(b"\0")[:count]]
        elif typ == 4:  # INT32
            tags[tag] = list(struct.unpack(f">{count}I", store[off : off + 4 * count]))
        elif typ == 3:  # INT16
            tags[tag] = list(struct.unpack(f">{count}H", store[off : off + 2 * count]))
        else:  # BIN and the rest: only whether the tag is there (a signature is BIN)
            tags[tag] = None
    return tags, pos + 16 + 16 * nindex + hsize


def rpm(path: str, out: str) -> None:
    with open(path, "rb") as f:
        blob = f.read()
    if blob[:4] != b"\xed\xab\xee\xdb":
        fail("not an rpm")
    sig, end = rpm_header(blob, 96)
    end += (-end) % 8  # the signature header is padded to 8
    body = end
    tags, end = rpm_header(blob, end)
    if tags.get(1125) not in (None, "gzip"):  # PAYLOADCOMPRESSOR
        fail(f"payload compression {tags.get(1125)} (expected gzip)")
    print(f"name {tags.get(1000, '')}")
    print(f"version {tags.get(1001, '')}-{tags.get(1002, '')}")
    print(f"arch {tags.get(1022, '')}")
    # PREIN, POSTIN, PREUN, POSTUN, TRIGGERSCRIPTS, PRETRANS, POSTTRANS, VERIFYSCRIPT, the file
    # triggers and the transaction file triggers.
    for tag in (1023, 1024, 1025, 1026, 1065, 1151, 1152, 1079, 5066, 5076):
        if tag in tags:
            print(f"script rpm tag {tag}")
    for req in tags.get(1049, []):
        if not req.startswith("rpmlib("):
            print(f"depends {req}")
    # RSA (268) or DSA (267) over the header, or rpm 6's OpenPGP signatures (278).
    print("signed " + ("yes" if any(t in sig for t in (267, 268, 278)) else "no"))
    print(f"body {hashlib.sha256(blob[body:]).hexdigest()}")
    dirnames, basenames, dirindexes = tags.get(1118, []), tags.get(1117, []), tags.get(1116, [])
    users, groups = tags.get(1039, []), tags.get(1040, [])
    owner_of = {}
    for i, b in enumerate(basenames):
        owner_of[dirnames[dirindexes[i]] + b] = f"{users[i]}:{groups[i]}"
    cpio = gzip.decompress(blob[end:])
    pos = 0
    while True:
        h = cpio[pos : pos + 110]
        if h[:6] != b"070701":
            fail("a malformed cpio header (expected newc)")
        mode = int(h[14:22], 16)
        size = int(h[54:62], 16)
        namesize = int(h[94:102], 16)
        name = cpio[pos + 110 : pos + 110 + namesize - 1].decode("utf-8")
        pos += 110 + namesize
        pos += (-pos) % 4
        data = cpio[pos : pos + size]
        pos += size
        pos += (-pos) % 4
        if name == "TRAILER!!!":
            break
        kind = mode & 0o170000
        _, shown = safe_join(out, name)
        owner = owner_of.get(shown, "?:?")
        if kind == 0o040000:
            print(f"dir {mode & 0o7777:o} {owner} {shown}")
        elif kind == 0o100000:
            write_file(out, name, data)
            print(f"file {mode & 0o7777:o} {owner} {shown}")
        else:
            fail(f"{name}: neither a file nor a directory")


def main() -> None:
    if len(sys.argv) != 3:
        fail("usage: pkg_inspect.py PACKAGE OUTDIR")
    pkg, out = sys.argv[1], sys.argv[2]
    os.makedirs(out, exist_ok=True)
    if pkg.endswith(".deb"):
        deb(pkg, out)
    elif pkg.endswith(".rpm"):
        rpm(pkg, out)
    else:
        fail(f"{pkg}: neither .deb nor .rpm")


if __name__ == "__main__":
    main()
