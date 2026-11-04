"""Synthetic .deb and .rpm files for `just release-test` (selftest.sh), so that release-check's
package checks run without Docker or nfpm. They have the structure pkg_inspect.py reads, and no more:
the real packages come from nfpm (linux-packages.sh), which the Docker tests and a release dry run
exercise.

    python3 -I selftest_pkg.py deb|rpm OUT NAME VERSION RELEASE ARCH OWNER MANIFEST
    python3 -I selftest_pkg.py sign RPM      add a (fake) RSA signature to the signature header
    python3 -I selftest_pkg.py tamper RPM    add one to the last byte of the payload

MANIFEST has one line per file, `SRC<TAB>DST<TAB>MODE` (octal), and for an rpm's own directory
`-<TAB>DST<TAB>755`; OWNER is the uid and gid of every entry (0 for root). SELFTEST_PKG_DEPENDS and
SELFTEST_PKG_SCRIPT, if set, add a dependency and a maintainer script.
"""

import os

import gzip
import io
import struct
import sys
import tarfile


def manifest(path: str):
    with open(path, encoding="utf-8") as f:
        for line in f:
            if line.strip():
                src, dst, mode = line.rstrip("\n").split("\t")
                yield src, dst, int(mode, 8)


def files_only(entries):
    return [e for e in entries if e[0] != "-"]


def dirs_of(entries) -> list:
    out = set()
    for _, dst, _ in entries:
        parts = dst.strip("/").split("/")[:-1]
        for i in range(1, len(parts) + 1):
            out.add("/" + "/".join(parts[:i]))
    return sorted(out)


def tar_gz(members) -> bytes:
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz", format=tarfile.GNU_FORMAT) as t:
        for name, data, mode, owner in members:
            info = tarfile.TarInfo(name)
            info.mode, info.uid, info.gid = mode, owner, owner
            info.uname = info.gname = "root" if owner == 0 else ""
            if data is None:
                info.type = tarfile.DIRTYPE
                t.addfile(info)
            else:
                info.size = len(data)
                t.addfile(info, io.BytesIO(data))
    return buf.getvalue()


def ar(members) -> bytes:
    out = b"!<arch>\n"
    for name, data in members:
        out += f"{name:<16}{0:<12}{0:<6}{0:<6}{0o100644:<8o}{len(data):<10}`\n".encode()
        out += data + (b"\n" if len(data) & 1 else b"")
    return out


def deb(out, name, version, release, arch, owner, entries) -> None:
    entries = files_only(entries)
    control = f"Package: {name}\nVersion: {version}-{release}\nArchitecture: {arch}\n"
    if os.environ.get("SELFTEST_PKG_DEPENDS"):
        control += f"Depends: {os.environ['SELFTEST_PKG_DEPENDS']}\n"
    control_members = [("./control", control.encode(), 0o644, 0)]
    if os.environ.get("SELFTEST_PKG_SCRIPT"):
        control_members.append(("./postinst", b"#!/bin/sh\n", 0o755, 0))
    data = [("./" + d.lstrip("/"), None, 0o755, owner) for d in dirs_of(entries)]
    for src, dst, mode in entries:
        with open(src, "rb") as f:
            data.append(("./" + dst.lstrip("/"), f.read(), mode, owner))
    blob = ar(
        [
            ("debian-binary", b"2.0\n"),
            ("control.tar.gz", tar_gz(control_members)),
            ("data.tar.gz", tar_gz(data)),
        ]
    )
    with open(out, "wb") as f:
        f.write(blob)


def header(tags) -> bytes:
    index, store = b"", b""
    for tag, typ, value in tags:
        if typ == 6:
            data, count = value.encode() + b"\0", 1
        elif typ == 8:
            data, count = b"".join(v.encode() + b"\0" for v in value), len(value)
        else:  # 4: INT32
            while len(store) % 4:
                store += b"\0"
            data, count = struct.pack(f">{len(value)}I", *value), len(value)
        index += struct.pack(">IIII", tag, typ, len(store), count)
        store += data
    return b"\x8e\xad\xe8\x01\0\0\0\0" + struct.pack(">II", len(tags), len(store)) + index + store


def cpio(entries) -> bytes:
    out = b""
    for i, (name, data, mode) in enumerate(entries + [("TRAILER!!!", b"", 0)]):
        n = name.encode() + b"\0"
        fields = [i + 1, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(n), 0]
        h = b"070701" + b"".join(f"{v:08x}".encode() for v in fields)
        out += h + n
        out += b"\0" * ((-len(out)) % 4)
        out += data
        out += b"\0" * ((-len(out)) % 4)
    return out


def rpm(out, name, version, release, arch, owner, entries) -> None:
    files = []
    for src, dst, mode in entries:
        if src == "-":
            files.append((dst, b"", 0o040000 | mode))
            continue
        with open(src, "rb") as f:
            files.append((dst, f.read(), 0o100000 | mode))
    dirnames = sorted({d.rsplit("/", 1)[0] + "/" for d, _, _ in files})
    user = "root" if owner == 0 else str(owner)
    tags = [
        (1000, 6, name),
        (1001, 6, version),
        (1002, 6, release),
        (1022, 6, arch),
        (1039, 8, [user] * len(files)),
        (1040, 8, [user] * len(files)),
        (1116, 4, [dirnames.index(d.rsplit("/", 1)[0] + "/") for d, _, _ in files]),
        (1117, 8, [d.rsplit("/", 1)[1] for d, _, _ in files]),
        (1118, 8, dirnames),
        (1125, 6, "gzip"),
    ]
    if os.environ.get("SELFTEST_PKG_DEPENDS"):
        tags.append((1049, 8, [os.environ["SELFTEST_PKG_DEPENDS"]]))
    if os.environ.get("SELFTEST_PKG_SCRIPT"):
        tags.append((1024, 6, "true"))
    tags.sort()
    lead = b"\xed\xab\xee\xdb\x03\x00" + b"\0" * 90
    sig = header([])
    sig += b"\0" * ((-len(sig)) % 8)
    payload = gzip.compress(cpio([("." + d, data, mode) for d, data, mode in files]), mtime=0)
    with open(out, "wb") as f:
        f.write(lead + sig + header(tags) + payload)


def sign(path: str) -> None:
    """Insert a BIN tag 268 (RSA) into the signature header; the rest of the file is unchanged."""
    with open(path, "rb") as f:
        blob = f.read()
    pos = 96
    nindex, hsize = struct.unpack(">II", blob[pos + 8 : pos + 16])
    index = blob[pos + 16 : pos + 16 + 16 * nindex]
    store = blob[pos + 16 + 16 * nindex : pos + 16 + 16 * nindex + hsize]
    end = pos + 16 + 16 * nindex + hsize
    end += (-end) % 8
    index += struct.pack(">IIII", 268, 7, len(store), 8)
    store += b"FAKE-SIG"
    sig = b"\x8e\xad\xe8\x01\0\0\0\0" + struct.pack(">II", nindex + 1, len(store)) + index + store
    sig += b"\0" * ((-len(sig)) % 8)
    with open(path, "wb") as f:
        f.write(blob[:96] + sig + blob[end:])


def tamper(path: str) -> None:
    with open(path, "rb") as f:
        blob = bytearray(f.read())
    blob[-1] = (blob[-1] + 1) % 256
    with open(path, "wb") as f:
        f.write(blob)


def main() -> None:
    if sys.argv[1] in ("sign", "tamper"):
        (sign if sys.argv[1] == "sign" else tamper)(sys.argv[2])
        return
    kind, out, name, version, release, arch, owner, man = sys.argv[1:9]
    entries = list(manifest(man))
    (deb if kind == "deb" else rpm)(out, name, version, release, arch, int(owner), entries)


if __name__ == "__main__":
    main()
