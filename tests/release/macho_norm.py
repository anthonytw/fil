#!/usr/bin/env python3
"""A portable Mach-O normalizer for the macOS release (docs/m4/release.md section 10.3).

Run it as `python3 -I macho_norm.py COMMAND ...`; standard library only, any OS (Linux too).

CI signs the macOS binary ad hoc. The owner signs it again with the Developer ID. A signature is
the only difference between the two, but removing it with `codesign --remove-signature` does not
give the same bytes twice: the signer also sets the `__LINKEDIT` segment's file size and VM size
from the size of the signature it wrote, a fat file's slice sizes follow, and different `codesign`
versions choose different page sizes and reserve different space. So the two cannot be compared
with codesign, and CI's signature cannot be reproduced by anyone else's. This tool does the
comparison itself, from the bytes alone. For every slice of a thin or fat 64-bit little-endian
Mach-O it:

  * cuts the slice at LC_CODE_SIGNATURE's dataoff (the signature is the last thing in the file;
    anything else there is refused; no segment but __LINKEDIT and no load command may reach into it,
    and __LINKEDIT's vmsize must be its file size rounded up to a page),
  * removes the LC_CODE_SIGNATURE load command (later commands move up, ncmds and sizeofcmds drop,
    the freed bytes are zero, as they are in a binary that was never signed),
  * sets __LINKEDIT's filesize to end where the signature began, and its vmsize to that rounded up to
    the architecture's page (16 KiB for arm64, 4 KiB otherwise),
  * and, for a fat file, lays the slices out again (the architecture's alignment, zero padding) with the
    new sizes.

The same code gives the same bytes whether it came from the linker, from CI's ad hoc `codesign`
(any version, any page size) or from a Developer ID signature. Anything it cannot parse, or that does
not look as expected (an unknown magic, a signature that is not last, bytes it does not account for,
non-zero padding), is REFUSED with exit status 2, never passed.

Commands:
  normalize IN OUT       write the normalized form of IN
  sha256 IN              the sha256 of the normalized form of IN
  same A B               exit 0 if A and B normalize to the same bytes; else say how they differ, exit 1
  info IN                one line per slice: kind (adhoc, signed, unsigned), identifier, flags, team,
                         entitlements, and so on, as key=value (what the signature grants)
  archives CI SIGNED     the release archive SIGNED equals CI (the attested archive) in every member
                         and every piece of tar metadata but bin/fil, and its bin/fil normalizes to
                         the same bytes as CI's
"""
import hashlib
import struct
import sys
import tarfile

FAT_MAGIC, FAT_MAGIC_64 = 0xCAFEBABE, 0xCAFEBABF
MH_MAGIC_64 = 0xFEEDFACF
LC_SEGMENT_64, LC_CODE_SIGNATURE = 0x19, 0x1D
CPU_ARM64 = 0x0100000C
CSMAGIC_EMBEDDED_SIGNATURE, CSMAGIC_CODEDIRECTORY = 0xFADE0CC0, 0xFADE0C02
CSMAGIC_ENTITLEMENTS, CSMAGIC_DER_ENTITLEMENTS, CSMAGIC_CMS = 0xFADE7171, 0xFADE7172, 0xFADE0B01
CS_ADHOC, CS_LINKER_SIGNED = 0x2, 0x20000


class Refused(Exception):
    pass


def refuse(msg):
    raise Refused(msg)


def round_up(n, a):
    return (n + a - 1) // a * a


def u32(b, o):
    if o < 0 or o + 4 > len(b):
        refuse("read past the end of the data")
    return struct.unpack_from("<I", b, o)[0]


def u64(b, o):
    if o < 0 or o + 8 > len(b):
        refuse("read past the end of the data")
    return struct.unpack_from("<Q", b, o)[0]


def be32(b, o):
    if o < 0 or o + 4 > len(b):
        refuse("signature blob: read past the end")
    return struct.unpack_from(">I", b, o)[0]


def parse_slice(s):
    """-> dict: header values, load commands, the signature command, __LINKEDIT."""
    if len(s) < 32 or u32(s, 0) != MH_MAGIC_64:
        refuse("slice is not a 64-bit little-endian Mach-O (only x86_64 and arm64 are handled)")
    cputype, ncmds, sizeofcmds = u32(s, 4), u32(s, 16), u32(s, 20)
    if 32 + sizeofcmds > len(s):
        refuse("load commands run past the end of the slice")
    lcs, off, sig, linkedit = [], 32, None, None
    for i in range(ncmds):
        if off + 8 > 32 + sizeofcmds:
            refuse("load command %d is outside the load command area" % i)
        cmd, size = u32(s, off), u32(s, off + 4)
        if size < 8 or size % 8 or off + size > 32 + sizeofcmds:
            refuse("load command %d has a bad size %d" % (i, size))
        lcs.append((off, cmd, size))
        if cmd == LC_CODE_SIGNATURE:
            if sig is not None:
                refuse("two LC_CODE_SIGNATURE commands")
            if size != 16:
                refuse("LC_CODE_SIGNATURE has size %d, not 16" % size)
            sig = {"lc": off, "dataoff": u32(s, off + 8), "datasize": u32(s, off + 12)}
        elif cmd == LC_SEGMENT_64:
            if size < 72:
                refuse("LC_SEGMENT_64 is too small")
            if s[off + 8 : off + 24].rstrip(b"\0") == b"__LINKEDIT":
                if linkedit is not None:
                    refuse("two __LINKEDIT segments")
                linkedit = {
                    "lc": off,
                    "vmsize": u64(s, off + 32),
                    "fileoff": u64(s, off + 40),
                    "filesize": u64(s, off + 48),
                }
        off += size
    if off != 32 + sizeofcmds:
        refuse("the load commands do not fill sizeofcmds")
    if linkedit is None:
        refuse("no __LINKEDIT segment")
    return {"cputype": cputype, "ncmds": ncmds, "sizeofcmds": sizeofcmds, "lcs": lcs, "sig": sig, "linkedit": linkedit}


# load commands that point into __LINKEDIT: (cmd, [(offset field, size field or None)]) at byte offsets
# of the command; a size of None means the offset alone is checked
LINKEDIT_REFS = {
    0x2: [(8, 12), (16, 20)],  # LC_SYMTAB: symoff/nsyms, stroff/strsize
    0xB: [(32 + 4 * i, 36 + 4 * i) for i in (0, 2, 4, 6, 8, 10)],  # LC_DYSYMTAB: toc, modtab, extrefsym, indirectsym, extrel, locrel
    0x22: [(8 + 8 * i, 12 + 8 * i) for i in range(5)],  # LC_DYLD_INFO: rebase, bind, weak bind, lazy bind, export
    0x80000022: [(8 + 8 * i, 12 + 8 * i) for i in range(5)],
}
for _c in (0x1E, 0x26, 0x29, 0x2B, 0x2E, 0x80000033, 0x80000034):  # linkedit_data_command
    LINKEDIT_REFS[_c] = [(8, 12)]


def check_inert_signature(s, p, dataoff):
    """The signature must be inert data: no segment but __LINKEDIT and no load command reaches into it."""
    for off, cmd, size in p["lcs"]:
        if cmd == LC_SEGMENT_64 and s[off + 8 : off + 24].rstrip(b"\0") != b"__LINKEDIT":
            if u64(s, off + 40) + u64(s, off + 48) > dataoff:
                refuse("a segment reaches into the signature")
        for o_off, o_size in LINKEDIT_REFS.get(cmd, ()):
            if o_off + 4 > size or o_size + 4 > size:
                continue
            start, length = u32(s, off + o_off), u32(s, off + o_size)
            if cmd in (0xB,):
                # DYSYMTAB: (offset, entry count): an offset past the signature's start is enough to refuse
                if length and start >= dataoff:
                    refuse("a load command refers to data in or after the signature")
            elif length and start + length > dataoff:
                refuse("a load command refers to data in or after the signature")


def normalize_slice(s):
    p = parse_slice(s)
    if p["sig"] is None:
        # Never signed: already in the unsigned form, but nothing may follow __LINKEDIT.
        le = p["linkedit"]
        if le["fileoff"] + le["filesize"] != len(s):
            refuse("an unsigned slice whose __LINKEDIT does not end the slice")
        return bytes(s)
    sig, le = p["sig"], p["linkedit"]
    end = sig["dataoff"] + sig["datasize"]
    if end != len(s):
        refuse("the signature (ends at %d) is not the end of the slice (%d)" % (end, len(s)))
    if le["fileoff"] + le["filesize"] != end:
        refuse("the signature is not the end of __LINKEDIT")
    if sig["dataoff"] < le["fileoff"]:
        refuse("the signature starts before __LINKEDIT")
    check_inert_signature(s, p, sig["dataoff"])
    # every signer sets vmsize to the signed file size rounded up to a page (4 KiB or 16 KiB): nothing else
    if le["vmsize"] not in (round_up(le["filesize"], 0x1000), round_up(le["filesize"], 0x4000)):
        refuse("__LINKEDIT's vmsize (0x%x) is not its file size (0x%x) rounded up to a page" % (le["vmsize"], le["filesize"]))
    out = bytearray(s[: sig["dataoff"]])
    # drop the LC_CODE_SIGNATURE command: later commands move up, the freed bytes become zero
    lc = sig["lc"]
    area_end = 32 + p["sizeofcmds"]
    out[lc : area_end - 16] = s[lc + 16 : area_end]
    out[area_end - 16 : area_end] = b"\0" * 16
    struct.pack_into("<I", out, 16, p["ncmds"] - 1)
    struct.pack_into("<I", out, 20, p["sizeofcmds"] - 16)
    # __LINKEDIT's position moved up if it came after the signature command
    le_off = le["lc"] - 16 if le["lc"] > lc else le["lc"]
    filesize = sig["dataoff"] - le["fileoff"]
    page = 0x4000 if p["cputype"] == CPU_ARM64 else 0x1000
    struct.pack_into("<Q", out, le_off + 32, round_up(filesize, page))
    struct.pack_into("<Q", out, le_off + 48, filesize)
    return bytes(out)


def split_fat(b):
    """-> [(cputype, cpusubtype, align, slice bytes)] for a fat file, or None for a thin one."""
    if len(b) < 8:
        refuse("file too short")
    magic = struct.unpack_from(">I", b, 0)[0]
    if magic not in (FAT_MAGIC, FAT_MAGIC_64):
        return None
    n = struct.unpack_from(">I", b, 4)[0]
    if n == 0 or n > 8:
        refuse("a fat file with %d architectures" % n)
    wide = magic == FAT_MAGIC_64
    esize = 32 if wide else 20
    if 8 + n * esize > len(b):
        refuse("fat header runs past the end of the file")
    archs, cursor = [], 8 + n * esize
    for i in range(n):
        e = 8 + i * esize
        if wide:
            cpu, sub, off, size, align, _ = struct.unpack_from(">iiQQII", b, e)
        else:
            cpu, sub, off, size, align = struct.unpack_from(">iiIII", b, e)
        if align > 16 or off < cursor or off + size > len(b) or size == 0:
            refuse("fat architecture %d is out of place" % i)
        if b[cursor:off].strip(b"\0"):
            refuse("non-zero bytes between fat slices")
        archs.append((cpu & 0xFFFFFFFF, sub & 0xFFFFFFFF, align, b[off : off + size]))
        cursor = off + size
    if cursor != len(b):
        refuse("%d bytes after the last fat slice" % (len(b) - cursor))
    return archs, wide


def normalize(b):
    f = split_fat(b)
    if f is None:
        return normalize_slice(b)
    archs, wide = f
    for cpu, _sub, _align, s in archs:
        if parse_slice(s)["cputype"] != cpu:
            refuse("a fat architecture's cputype (0x%x) is not its slice's (0x%x)" % (cpu, parse_slice(s)["cputype"]))
    parts = [(cpu, sub, align, normalize_slice(s)) for cpu, sub, align, s in archs]
    esize = 32 if wide else 20
    out = bytearray(struct.pack(">II", FAT_MAGIC_64 if wide else FAT_MAGIC, len(parts)))
    cursor = 8 + len(parts) * esize
    body = bytearray()
    base = cursor

    def signed(v):
        return v - (1 << 32) if v >= 1 << 31 else v

    for cpu, sub, _align, s in parts:
        # the slice alignment is the architecture's page (2^14 for arm64, 2^12 otherwise): codesign
        # and lipo do not agree on it either
        align = 14 if cpu == CPU_ARM64 else 12
        off = round_up(cursor, 1 << align)
        body += b"\0" * (off - cursor)
        body += s
        cursor = off + len(s)
        if wide:
            out += struct.pack(">iiQQII", signed(cpu), signed(sub), off, len(s), align, 0)
        else:
            out += struct.pack(">iiIII", signed(cpu), signed(sub), off, len(s), align)
    return bytes(out) + bytes(body)


def info_slice(s):
    p = parse_slice(s)
    r = {"cputype": "0x%x" % p["cputype"], "kind": "unsigned", "identifier": "", "flags": "", "team": "", "entitlements": "no", "cms": "no", "pagesize": ""}
    sig = p["sig"]
    if sig is None:
        return r
    sb = s[sig["dataoff"] : sig["dataoff"] + sig["datasize"]]
    if be32(sb, 0) != CSMAGIC_EMBEDDED_SIGNATURE:
        refuse("the signature is not an embedded-signature superblob")
    length, count = be32(sb, 4), be32(sb, 8)
    if length > len(sb) or count > 64:
        refuse("the signature superblob is malformed")
    cd = None
    for i in range(count):
        typ, boff = be32(sb, 12 + 8 * i), be32(sb, 16 + 8 * i)
        magic = be32(sb, boff)
        if typ == 0:
            if magic != CSMAGIC_CODEDIRECTORY:
                refuse("slot 0 is not a CodeDirectory")
            cd = boff
        elif magic in (CSMAGIC_ENTITLEMENTS, CSMAGIC_DER_ENTITLEMENTS) or typ in (5, 7):
            r["entitlements"] = "yes"
        elif magic == CSMAGIC_CMS:
            # an ad hoc signature carries an empty CMS blob (8 bytes: magic and length)
            r["cms"] = "yes" if be32(sb, boff + 4) > 8 else "empty"
    if cd is None:
        refuse("the signature has no CodeDirectory")
    version, flags = be32(sb, cd + 8), be32(sb, cd + 12)
    ident_off = be32(sb, cd + 20)
    end = sb.index(b"\0", cd + ident_off)
    r["identifier"] = sb[cd + ident_off : end].decode("ascii", "replace")
    r["flags"] = "0x%x" % flags
    r["pagesize"] = str(1 << sb[cd + 39]) if sb[cd + 39] else "0"
    if version >= 0x20200:
        toff = be32(sb, cd + 48)
        if toff:
            r["team"] = sb[cd + toff : sb.index(b"\0", cd + toff)].decode("ascii", "replace")
    if flags & CS_ADHOC:
        r["kind"] = "adhoc"
    else:
        r["kind"] = "signed" if r["cms"] == "yes" else "unsigned-or-malformed"
    if flags & CS_LINKER_SIGNED:
        r["linker"] = "yes"
    return r


def slices_of(b):
    f = split_fat(b)
    return [b] if f is None else [a[3] for a in f[0]]


def read(path):
    with open(path, "rb") as f:
        return f.read()


def diff_report(a, b):
    n = sum(1 for x, y in zip(a, b) if x != y) + abs(len(a) - len(b))
    return "%d bytes differ (sizes %d and %d)" % (n, len(a), len(b))


def cmd_same(a, b):
    na, nb = normalize(read(a)), normalize(read(b))
    if na == nb:
        print("identical after normalization (sha256 %s)" % hashlib.sha256(na).hexdigest())
        return 0
    print("NOT identical after normalization: " + diff_report(na, nb))
    try:
        sa, sb = slices_of(na), slices_of(nb)
        for i, (x, y) in enumerate(zip(sa, sb)):
            print("  slice %d (cputype 0x%x): %s" % (i, parse_slice(x)["cputype"], "identical" if x == y else "DIFFERS: " + diff_report(x, y)))
    except Refused:
        pass
    return 1


def member_key(m):
    return (m.name, m.type, m.mode, m.uid, m.gid, m.uname, m.gname, m.mtime, m.linkname)


def tar_headers(raw):
    """The (typeflag, magic) of every 512-byte tar header in RAW, including pax and GNU long-name ones."""
    out, pos = [], 0
    while pos + 512 <= len(raw) and raw[pos : pos + 512].strip(b"\0"):
        size = int(raw[pos + 124 : pos + 136].strip(b"\0 ") or b"0", 8)
        out.append((raw[pos + 156 : pos + 157], raw[pos + 257 : pos + 265]))
        pos += 512 + round_up(size, 512)
    return out


def cmd_archives(ci, signed):
    import gzip

    def load(p):
        out, names = {}, []
        with gzip.open(p, "rb") as g:
            raw = g.read()
        import io

        with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as t:
            for m in t.getmembers():
                names.append(m.name)
                out[m.name] = (m, t.extractfile(m).read() if m.isfile() else None)
            fmt, end = tar_headers(raw), t.offset
        if raw[end:].strip(b"\0"):
            refuse("%s: data after the end of the tar archive" % p)
        return names, out, fmt

    n1, a, f1 = load(ci)
    n2, b, f2 = load(signed)
    if f1 != f2:
        refuse("the tar formats or extended headers differ")
    if n1 != n2:
        refuse("the member lists differ: %s" % sorted(set(n1) ^ set(n2))[:6])
    bins = [n for n in n1 if n.endswith("/bin/fil")]
    if len(bins) != 1:
        refuse("expected exactly one bin/fil, found %d" % len(bins))
    binname = bins[0]
    for n in n1:
        (m1, d1), (m2, d2) = a[n], b[n]
        # tar metadata, including the extended (pax) headers that carry xattrs and ACLs
        if member_key(m1) != member_key(m2) or m1.pax_headers != m2.pax_headers:
            refuse("%s: tar metadata differs (mode, owner, mtime, extended headers)" % n)
        if n != binname and d1 != d2:
            refuse("%s differs between the CI archive and the signed archive" % n)
    na, nb = normalize(a[binname][1]), normalize(b[binname][1])
    if na != nb:
        refuse("%s differs by more than its signature: %s" % (binname, diff_report(na, nb)))
    print("every member but %s is identical; %s is identical once its signature is normalized away (sha256 %s)" % (binname, binname, hashlib.sha256(na).hexdigest()))
    return 0


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    try:
        c = argv[1]
        if c == "normalize" and len(argv) == 4:
            with open(argv[3], "wb") as f:
                f.write(normalize(read(argv[2])))
            return 0
        if c == "sha256" and len(argv) == 3:
            print(hashlib.sha256(normalize(read(argv[2]))).hexdigest())
            return 0
        if c == "same" and len(argv) == 4:
            return cmd_same(argv[2], argv[3])
        if c == "info" and len(argv) == 3:
            for i, s in enumerate(slices_of(read(argv[2]))):
                print("slice=%d " % i + " ".join("%s=%s" % kv for kv in info_slice(s).items()))
            return 0
        if c == "archives" and len(argv) == 4:
            return cmd_archives(argv[2], argv[3])
        print("usage: see the top of this file", file=sys.stderr)
        return 2
    except Refused as e:
        print("macho_norm: REFUSED: %s" % e, file=sys.stderr)
        return 2
    except (OSError, tarfile.TarError, struct.error, ValueError) as e:
        print("macho_norm: REFUSED: %s" % e, file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
