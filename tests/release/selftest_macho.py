#!/usr/bin/env python3
"""Fixtures for `just release-test` (selftest.sh): synthetic fat Mach-O binaries with real signature
structure (superblob, CodeDirectory, load command, __LINKEDIT sizes), and a fake `codesign` that
signs them. Run as `python3 -I selftest_macho.py COMMAND ...`; standard library only.

  build OUT SEED [--linker]       a fat file: an x86_64 slice (unsigned) and an arm64 slice, either
                                  unsigned or (--linker) signed as ld64 does (identifier fil-HASH, flags
                                  0x20002, 4 KiB pages, the signature command in the middle)
  codesign ARGS...                the subset of codesign(1) that sign-macos.sh and verify use:
                                  --force --options runtime --timestamp[=none] --identifier I --sign ID
                                  [--pagesize N] FILE | --verify ... FILE | -dvv FILE
"""
import os
import struct
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import macho_norm as mn  # noqa: E402

CPU_X86_64 = 0x01000007


def build_slice(seed, cputype, pagesize):
    text_len = 0x2000 + seed * 16
    le_off = mn.round_up(0x200 + text_len, pagesize)
    le_len = 0x1010
    cmds = b""
    seg = lambda name, vmaddr, vmsize, fileoff, filesize: (
        struct.pack("<II16sQQQQiiII", mn.LC_SEGMENT_64, 72, name, vmaddr, vmsize, fileoff, filesize, 7, 5, 0, 0))
    cmds += seg(b"__TEXT", 0x100000000, mn.round_up(le_off, pagesize), 0, le_off)
    cmds += seg(b"__LINKEDIT", 0x100000000 + mn.round_up(le_off, pagesize), mn.round_up(le_len, pagesize), le_off, le_len)
    cmds += struct.pack("<II16s", 0x1B, 24, bytes([seed] * 16))  # LC_UUID
    hdr = struct.pack("<IiiIIIII", mn.MH_MAGIC_64, cputype - (1 << 32) if cputype >= 1 << 31 else cputype, 0, 2, 3, len(cmds), 0, 0)
    body = bytearray(le_off + le_len)
    body[: len(hdr) + len(cmds)] = hdr + cmds
    code = (b"CODE-%d-" % seed) * ((text_len // 8) + 1)
    body[0x200 : 0x200 + text_len] = code[:text_len]
    body[le_off : le_off + le_len] = ((b"LINK%d" % seed) * le_len)[:le_len]
    return bytes(body)


def superblob(ident, flags, team, pagesize, code_limit, cms, ents):
    npages = (code_limit + pagesize - 1) // pagesize
    ident_b = ident.encode() + b"\0"
    team_b = team.encode() + b"\0" if team else b""
    ident_off = 88
    team_off = ident_off + len(ident_b) if team else 0
    hash_off = ident_off + len(ident_b) + len(team_b) + 2 * 32  # two special slots
    cd = struct.pack(">IIIIIIIIIBBBBIIIII", mn.CSMAGIC_CODEDIRECTORY, 0, 0x20400, flags, hash_off, ident_off, 2, npages, code_limit,
                     32, 2, 0, pagesize.bit_length() - 1, 0, 0, team_off, 0, 0)
    cd += b"\0" * (ident_off - len(cd)) + ident_b + team_b + b"\x11" * (2 * 32) + b"\x22" * (32 * npages)
    cd = cd[:4] + struct.pack(">I", len(cd)) + cd[8:]
    blobs = [(0, cd), (2, struct.pack(">II", 0xFADE0C01, 12) + b"\0\0\0\0")]
    if ents:
        blobs.append((5, struct.pack(">II", mn.CSMAGIC_ENTITLEMENTS, 8 + 12) + b"<plist/>\0\0\0\0"))
    blobs.append((0x10000, struct.pack(">II", mn.CSMAGIC_CMS, 8 + len(cms)) + cms))
    off = 12 + 8 * len(blobs)
    idx, data = b"", b""
    for typ, b in blobs:
        idx += struct.pack(">II", typ, off + len(data))
        data += b
    return struct.pack(">III", mn.CSMAGIC_EMBEDDED_SIGNATURE, off + len(data), len(blobs)) + idx + data


def sign_slice(s, ident, flags, team, pagesize, cms, ents, lc_middle=False, vm_page=None):
    base = mn.normalize_slice(s)
    p = mn.parse_slice(base)
    if 32 + p["sizeofcmds"] + 16 > 0x200 or base[32 + p["sizeofcmds"] : 32 + p["sizeofcmds"] + 16].strip(b"\0"):
        raise SystemExit("no room for a load command")
    le = p["linkedit"]
    dataoff = mn.round_up(len(base), 16)
    sb = superblob(ident, flags, team, pagesize, dataoff, cms, ents)
    out = bytearray(base + b"\0" * (dataoff - len(base)) + sb)
    area_end = 32 + p["sizeofcmds"]
    lc = struct.pack("<IIII", mn.LC_CODE_SIGNATURE, 16, dataoff, len(sb))
    if lc_middle:
        last = p["lcs"][-1][0]
        out[last + 16 : area_end + 16] = bytes(base[last:area_end])
        out[last : last + 16] = lc
        le_off = le["lc"] + (16 if le["lc"] >= last else 0)
    else:
        out[area_end : area_end + 16] = lc
        le_off = le["lc"]
    struct.pack_into("<I", out, 16, p["ncmds"] + 1)
    struct.pack_into("<I", out, 20, p["sizeofcmds"] + 16)
    filesize = len(out) - le["fileoff"]
    struct.pack_into("<Q", out, le_off + 32, mn.round_up(filesize, vm_page or pagesize))
    struct.pack_into("<Q", out, le_off + 48, filesize)
    return bytes(out)


def fat(slices, aligns):
    out = bytearray(struct.pack(">II", mn.FAT_MAGIC, len(slices)))
    cursor, body = 8 + 20 * len(slices), bytearray()
    for (cpu, s), al in zip(slices, aligns):
        off = mn.round_up(cursor, 1 << al)
        body += b"\0" * (off - cursor) + s
        cursor = off + len(s)
        out += struct.pack(">iiIII", cpu - (1 << 32) if cpu >= 1 << 31 else cpu, 0, off, len(s), al)
    return bytes(out) + bytes(body)


def slices_of(b):
    f = mn.split_fat(b)
    return [(a[0], a[3]) for a in f[0]]


def cmd_build(out, seed, linker):
    x86 = build_slice(seed, CPU_X86_64, 4096)
    arm = build_slice(seed + 1, mn.CPU_ARM64, 16384)
    if linker:
        arm = sign_slice(arm, "fil-%08x" % (seed * 2654435761 % (1 << 32)), 0x20002, "", 4096, b"", False, lc_middle=True)
    with open(out, "wb") as f:
        f.write(fat([(CPU_X86_64, x86), (mn.CPU_ARM64, arm)], [12, 14]))


def cms_text(identity, certteam, ts):
    return ("DEVID:%s|%s|ts=%s" % (identity, certteam, "yes" if ts else "no")).encode().ljust(3000, b"\0")


def cms_fields(b):
    text = b[b.index(b"DEVID:") :].split(b"\0")[0].decode()
    name, certteam, tsf = text[len("DEVID:") :].split("|")
    return name, certteam, tsf


def cmd_codesign(argv):
    mode, ident, opts, ts, idr, f, page, req = "sign", "", "", True, "fil", "", None, ""
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--verify":
            mode = "verify"
        elif a == "-dvv":
            mode = "display"
        elif a in ("--force", "--strict", "--verbose=2"):
            pass
        elif a == "--options":
            opts = argv[i + 1]; i += 1
        elif a == "--timestamp":
            ts = True
        elif a == "--timestamp=none":
            ts = False
        elif a == "--identifier":
            idr = argv[i + 1]; i += 1
        elif a == "--sign":
            ident = argv[i + 1]; i += 1
        elif a.startswith("-R="):
            req = a[3:]
        elif a == "--pagesize":
            page = int(argv[i + 1]); i += 1
        else:
            f = a
        i += 1
    env = os.environ
    b = open(f, "rb").read()
    if mode == "verify":
        if env.get("FAKE_CODESIGN_VERIFYBAD"):
            return 1
        try:
            infos = [mn.info_slice(s) for _, s in slices_of(b)]
        except mn.Refused:
            return 1
        if not all(x["kind"] in ("adhoc", "signed") for x in infos):
            return 1
        if req:
            # the designated requirement: a Developer ID signature whose CERTIFICATE (not the
            # CodeDirectory) names the team the requirement asks for
            if infos[0]["kind"] != "signed" or "certificate 1[field.1.2.840.113635.100.6.2.6]" not in req:
                return 3
            want = req.split('subject.OU] = "')[1].split('"')[0]
            return 0 if cms_fields(b)[1] == want else 3
        return 0
    if mode == "display":
        sys.stderr.write("Identifier=%s\n" % idr)
        infos = [mn.info_slice(s) for _, s in slices_of(b)]
        if infos[0]["kind"] == "unsigned":
            sys.stderr.write("%s: code object is not signed at all\n" % f)
            return 1
        sys.stderr.write("Format=Mach-O universal\nCodeDirectory v=20400 flags=%s\n" % infos[0]["flags"])
        if infos[0]["kind"] == "adhoc":
            sys.stderr.write("Signature=adhoc\n")
        else:
            name, _certteam, tsf = cms_fields(b)
            sys.stderr.write("Authority=%s\nTeamIdentifier=%s\n" % (name, infos[0]["team"]))
            if tsf == "ts=yes":
                sys.stderr.write("Timestamp=Nov 6, 2026\n")
        return 0
    target = f
    if env.get("FAKE_CODESIGN_WRONGFILE"):
        open(f + ".other", "wb").write(b)
        target = f + ".other"
    if env.get("FAKE_CODESIGN_IGNORE_IDENTITY"):
        ident = "-"
    adhoc = ident == "-"
    flags = 0x2 if adhoc else 0
    if opts == "runtime" and not env.get("FAKE_CODESIGN_NORUNTIME"):
        flags |= 0x10000
    team = "" if adhoc else env.get("FAKE_CODESIGN_TEAM", "6X3PT3FXGA")
    cms = b"" if adhoc else cms_text(ident, env.get("FAKE_CODESIGN_CERT_TEAM", team), ts)
    out = []
    for cpu, s in slices_of(b):
        if env.get("FAKE_CODESIGN_TAMPER") and not adhoc:
            s = bytearray(mn.normalize_slice(s)); s[0x300] ^= 1; s = bytes(s)
        pg = page or (4096 if adhoc else (16384 if cpu == mn.CPU_ARM64 else 4096))
        out.append((cpu, sign_slice(s, idr, flags, team, pg, cms, bool(env.get("FAKE_CODESIGN_ENTITLEMENTS")) and not adhoc, vm_page=pg)))
    open(target, "wb").write(fat(out, [14, 14]))
    return 0


def cmd_mutate(src, kind, out):
    """A deliberately malformed copy of a signed fat file (the normalizer must refuse it)."""
    b = bytearray(open(src, "rb").read())
    archs, _ = mn.split_fat(bytes(b))
    f = mn.split_fat(bytes(b))
    # file offset of the arm64 slice: the fat entries are 20 bytes after the 8-byte header
    entries = [struct.unpack_from(">iiIII", b, 8 + 20 * i) for i in range(len(archs))]
    arm = [e for e in entries if (e[0] & 0xFFFFFFFF) == mn.CPU_ARM64][0]
    base = arm[2]
    p = mn.parse_slice(bytes(b[base : base + arm[3]]))
    if kind == "vmsize":  # __LINKEDIT's vmsize 1 MiB too large
        struct.pack_into("<Q", b, base + p["linkedit"]["lc"] + 32, p["linkedit"]["vmsize"] + (1 << 20))
    elif kind == "segment":  # __TEXT reaches into the signature
        text = [o for o, c, sz in p["lcs"] if c == mn.LC_SEGMENT_64 and b[base + o + 8 : base + o + 14] == b"__TEXT"][0]
        struct.pack_into("<Q", b, base + text + 48, p["sig"]["dataoff"] + 16)
    elif kind == "ref":  # a linkedit-data command (LC_FUNCTION_STARTS) that points into the signature
        uuid = [o for o, c, sz in p["lcs"] if c == 0x1B][0]
        struct.pack_into("<IIII", b, base + uuid, 0x26, 24, p["sig"]["dataoff"], 8)
    elif kind == "cputype":  # the fat entry of the arm64 slice says x86_64
        i = entries.index(arm)
        struct.pack_into(">i", b, 8 + 20 * i, CPU_X86_64)
    else:
        raise SystemExit("unknown mutation")
    open(out, "wb").write(bytes(b))


def cmd_tarmutate(src, kind, out):
    """A copy of a release archive that differs in what tarfile's members do not show."""
    import gzip, io, tarfile
    raw = gzip.open(src, "rb").read()
    if kind == "trailing":
        raw = raw.rstrip(b"\0") + b"\0" * 1024 + b"extra data after the archive"
    elif kind == "pax":
        t = tarfile.open(fileobj=io.BytesIO(raw))
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as o:
            for m in t.getmembers():
                if m.name.endswith("README.md"):
                    m.pax_headers = {"SCHILY.xattr.user.x": "y"}
                o.addfile(m, t.extractfile(m) if m.isfile() else None)
        raw = buf.getvalue()
    with open(out, "wb") as f:
        with gzip.GzipFile(fileobj=f, mode="wb", mtime=0, filename="") as g:
            g.write(raw)


if __name__ == "__main__":
    if sys.argv[1] == "build":
        cmd_build(sys.argv[2], int(sys.argv[3]), "--linker" in sys.argv)
    elif sys.argv[1] == "mutate":
        cmd_mutate(sys.argv[2], sys.argv[3], sys.argv[4])
    elif sys.argv[1] == "tarmutate":
        cmd_tarmutate(sys.argv[2], sys.argv[3], sys.argv[4])
    elif sys.argv[1] == "codesign":
        sys.exit(cmd_codesign(sys.argv[2:]))
