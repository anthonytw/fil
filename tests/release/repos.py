"""Build fil's apt and dnf repository trees from a verified release's packages (docs/m4/packaging.md
section 7). Unsigned: repos.sh signs the two index files (Release, repodata/repomd.xml) with the
package key on the owner's machine, then checks the result.

    python3 -I repos.py --files DIR --version VERSION --out DIR [--base-url URL] [--epoch SECONDS]

DIR holds fil_VERSION_{amd64,arm64}.deb and fil-VERSION-1.{x86_64,aarch64}.rpm (the release's
files, which repos.sh has checked against the signed SHA256SUMS). OUT gets:

    apt/pool/main/f/fil/<the two .deb>
    apt/dists/stable/main/binary-{amd64,arm64}/Packages, Packages.gz
    apt/dists/stable/Release                      (repos.sh adds InRelease and Release.gpg)
    rpm/<the two .rpm>
    rpm/repodata/<sha256>-{primary,filelists,other}.xml.gz, repomd.xml   (repos.sh: repomd.xml.asc)
    rpm/fil.repo                                  (the dnf configuration users download)

Only this release's packages are in the trees: each run makes a complete repository of one version.
Every index binds each file by its SHA-256 (apt's Packages and Release, dnf's checksum and
repomd.xml), so the one signature over the top index covers every byte a client installs. The parsers
read what nfpm writes (a .deb of control.tar.gz and data.tar.gz; an rpm with a gzip cpio payload) and
refuse anything else. Standard library only; it runs nothing from the packages.
"""

import argparse
import gzip
import hashlib
import io
import os
import shutil
import struct
import sys
import tarfile
import time
from xml.sax.saxutils import escape, quoteattr

SUITE = "stable"
COMPONENT = "main"
DEB_ARCHES = {"amd64": "x86_64", "arm64": "aarch64"}


def fail(msg: str) -> None:
    sys.exit(f"repos: {msg}")


def sha(data: bytes, algo: str = "sha256") -> str:
    return hashlib.new(algo, data).hexdigest()


def gz(data: bytes) -> bytes:
    # mtime 0 and no file name: the same input gives the same bytes
    buf = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=buf, mtime=0, compresslevel=9) as g:
        g.write(data)
    return buf.getvalue()


def write(path: str, data: bytes) -> None:
    os.makedirs(os.path.dirname(path), mode=0o755, exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
    os.chmod(path, 0o644)


# ---------------------------------------------------------------- apt
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


def deb_control(blob: bytes) -> list:
    """The control file's paragraph as [(field, value-with-continuation-lines)], in order."""
    m = ar_members(blob)
    if m.get("debian-binary") != b"2.0\n" or "control.tar.gz" not in m:
        fail("expected a .deb with debian-binary 2.0 and control.tar.gz")
    with tarfile.open(fileobj=io.BytesIO(m["control.tar.gz"]), mode="r:gz") as t:
        f = t.extractfile("./control")
        if f is None:
            fail("no control file")
        text = f.read().decode("utf-8")
    fields = []
    for line in text.splitlines():
        if not line.strip():
            continue
        if line[0] in " \t":
            if not fields:
                fail("a continuation line before any field")
            fields[-1] = (fields[-1][0], fields[-1][1] + "\n" + line)
        elif ":" in line:
            k, v = line.split(":", 1)
            fields.append((k, v.strip()))
        else:
            fail(f"a malformed control line: {line!r}")
    return fields


def apt_tree(files: str, version: str, out: str, epoch: int) -> None:
    root = os.path.join(out, "apt")
    pool = f"pool/{COMPONENT}/f/fil"
    indexes = []
    for arch in sorted(DEB_ARCHES):
        name = f"fil_{version}_{arch}.deb"
        with open(os.path.join(files, name), "rb") as f:
            blob = f.read()
        fields = deb_control(blob)
        got = dict(fields)
        if got.get("Package") != "fil" or got.get("Architecture") != arch:
            fail(f"{name}: Package {got.get('Package')!r}, Architecture {got.get('Architecture')!r}")
        if got.get("Version", "").split("-")[0].replace("~", "-") != version:
            fail(f"{name}: Version {got.get('Version')!r} is not the release's {version}")
        write(os.path.join(root, pool, name), blob)
        # The control fields, then where the file is and its digests, with Description last, as
        # apt-ftparchive writes them.
        stanza = [f"{k}: {v}" for k, v in fields if k != "Description"]
        stanza += [
            f"Filename: {pool}/{name}",
            f"Size: {len(blob)}",
            f"MD5sum: {sha(blob, 'md5')}",
            f"SHA1: {sha(blob, 'sha1')}",
            f"SHA256: {sha(blob)}",
        ]
        if "Description" in got:
            stanza.append(f"Description: {got['Description']}")
        packages = ("\n".join(stanza) + "\n").encode("utf-8")
        rel = f"{COMPONENT}/binary-{arch}"
        write(os.path.join(root, "dists", SUITE, rel, "Packages"), packages)
        write(os.path.join(root, "dists", SUITE, rel, "Packages.gz"), gz(packages))
        indexes += [(f"{rel}/Packages", packages), (f"{rel}/Packages.gz", gz(packages))]
    date = time.strftime("%a, %d %b %Y %H:%M:%S UTC", time.gmtime(epoch))
    lines = [
        "Origin: fil",
        "Label: fil",
        f"Suite: {SUITE}",
        f"Codename: {SUITE}",
        f"Version: {version}",
        f"Date: {date}",
        "Architectures: " + " ".join(sorted(DEB_ARCHES)),
        f"Components: {COMPONENT}",
        "Description: fil's own packages, signed with the fil package signing key (https://github.com/anthonytw/fil)",
        "SHA256:",
    ]
    lines += [f" {sha(data)} {len(data):>8} {path}" for path, data in indexes]
    write(os.path.join(root, "dists", SUITE, "Release"), ("\n".join(lines) + "\n").encode("utf-8"))


# ---------------------------------------------------------------- dnf
def rpm_header(blob: bytes, pos: int):
    if blob[pos : pos + 3] != b"\x8e\xad\xe8":
        fail("a malformed rpm header")
    nindex, hsize = struct.unpack(">II", blob[pos + 8 : pos + 16])
    index = blob[pos + 16 : pos + 16 + 16 * nindex]
    store = blob[pos + 16 + 16 * nindex : pos + 16 + 16 * nindex + hsize]
    tags = {}
    for i in range(nindex):
        tag, typ, off, count = struct.unpack(">IIII", index[16 * i : 16 * i + 16])
        if typ in (6, 9):  # STRING, I18NSTRING (the first, the C locale's)
            tags[tag] = store[off:].split(b"\0", 1)[0].decode("utf-8")
        elif typ == 8:  # STRING_ARRAY
            tags[tag] = [s.decode("utf-8") for s in store[off:].split(b"\0")[:count]]
        elif typ == 5:  # INT64
            tags[tag] = list(struct.unpack(f">{count}Q", store[off : off + 8 * count]))
        elif typ == 4:  # INT32
            tags[tag] = list(struct.unpack(f">{count}I", store[off : off + 4 * count]))
        elif typ == 3:  # INT16
            tags[tag] = list(struct.unpack(f">{count}H", store[off : off + 2 * count]))
        else:
            tags[tag] = None
    return tags, pos + 16 + 16 * nindex + hsize


def cpio_entries(payload: bytes) -> list:
    """[(path, is_dir, size)] of a newc cpio archive."""
    out, pos = [], 0
    while True:
        h = payload[pos : pos + 110]
        if h[:6] != b"070701":
            fail("a malformed cpio header (expected newc)")
        mode = int(h[14:22], 16)
        size = int(h[54:62], 16)
        namesize = int(h[94:102], 16)
        name = payload[pos + 110 : pos + 110 + namesize - 1].decode("utf-8")
        pos += 110 + namesize
        pos += (-pos) % 4
        pos += size
        pos += (-pos) % 4
        if name == "TRAILER!!!":
            return out
        path = "/" + (name[2:] if name.startswith("./") else name).lstrip("/")
        if any(part in ("", ".", "..") for part in path[1:].split("/")):
            fail(f"an unsafe path in the payload: {name!r}")
        kind = mode & 0o170000
        if kind not in (0o040000, 0o100000):
            fail(f"{name}: neither a file nor a directory")
        out.append((path, kind == 0o040000, size))


FLAGS = {2: "LT", 4: "GT", 8: "EQ", 10: "LE", 12: "GE"}


def evr(s: str) -> tuple:
    e, _, vr = s.rpartition(":") if ":" in s else ("0", "", s)
    v, _, r = vr.partition("-")
    return e or "0", v, r


def deps(tags, names_tag, flags_tag, versions_tag, skip_rpmlib) -> str:
    names = tags.get(names_tag) or []
    flags = tags.get(flags_tag) or [0] * len(names)
    vers = tags.get(versions_tag) or [""] * len(names)
    out = []
    for n, fl, v in zip(names, flags, vers):
        if skip_rpmlib and n.startswith("rpmlib("):
            continue
        attrs = f"name={quoteattr(n)}"
        op = FLAGS.get(fl & 14)
        if op and v:
            e, ver, rel = evr(v)
            attrs += f' flags="{op}" epoch="{e}" ver={quoteattr(ver)}'
            if rel:
                attrs += f" rel={quoteattr(rel)}"
        out.append(f"      <rpm:entry {attrs}/>")
    return "\n".join(out)


def primary_file(path: str) -> bool:
    # what createrepo puts in primary.xml; the rest is in filelists.xml only
    return path.startswith("/etc/") or "bin/" in path or path == "/usr/lib/sendmail"


def rpm_package(files: str, name: str, epoch: int) -> dict:
    with open(os.path.join(files, name), "rb") as f:
        blob = f.read()
    if blob[:4] != b"\xed\xab\xee\xdb":
        fail(f"{name}: not an rpm")
    sig, end = rpm_header(blob, 96)
    end += (-end) % 8
    start = end
    tags, end = rpm_header(blob, end)
    if tags.get(1125) not in (None, "gzip"):
        fail(f"{name}: payload compression {tags.get(1125)} (expected gzip)")
    entries = cpio_entries(gzip.decompress(blob[end:]))
    epoch_tag = (tags.get(1003) or [0])[0]
    archivesize = (sig.get(271) or sig.get(1007) or tags.get(1046) or [0])[0]
    installed = (tags.get(5009) or tags.get(1009) or [sum(s for _, d, s in entries if not d)])[0]
    return {
        "file": name,
        "blob": blob,
        "pkgid": sha(blob),
        "name": tags.get(1000, ""),
        "arch": tags.get(1022, ""),
        "epoch": str(epoch_tag),
        "ver": tags.get(1001, ""),
        "rel": tags.get(1002, ""),
        "summary": tags.get(1004, ""),
        "description": tags.get(1005, ""),
        "packager": tags.get(1015, ""),
        "url": tags.get(1020, ""),
        "license": tags.get(1014, ""),
        "vendor": tags.get(1011, ""),
        "group": tags.get(1016, "Unspecified"),
        "buildhost": tags.get(1007, ""),
        "sourcerpm": tags.get(1044, ""),
        "buildtime": (tags.get(1006) or [0])[0],
        "filetime": epoch,
        "size": len(blob),
        "installed": installed,
        "archive": archivesize,
        "hstart": start,
        "hend": end,
        "provides": deps(tags, 1047, 1112, 1113, False),
        "requires": deps(tags, 1049, 1048, 1050, True),
        "entries": entries,
    }


def ver_xml(p) -> str:
    return f'<version epoch="{p["epoch"]}" ver={quoteattr(p["ver"])} rel={quoteattr(p["rel"])}/>'


def file_xml(path: str, is_dir: bool, indent: str) -> str:
    return f'{indent}<file type="dir">{escape(path)}</file>' if is_dir else f"{indent}<file>{escape(path)}</file>"


def dnf_tree(files: str, version: str, out: str, epoch: int, base_url: str) -> None:
    root = os.path.join(out, "rpm")
    pkgs = []
    for arch in ("aarch64", "x86_64"):
        name = f"fil-{version}-1.{arch}.rpm"
        p = rpm_package(files, name, epoch)
        if p["name"] != "fil" or p["arch"] != arch or p["ver"].replace("~", "-") != version or p["rel"] != "1":
            fail(f"{name}: name {p['name']!r}, arch {p['arch']!r}, version {p['ver']!r}-{p['rel']!r}")
        write(os.path.join(root, name), p["blob"])
        pkgs.append(p)
    n = len(pkgs)
    primary = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        f'<metadata xmlns="http://linux.duke.edu/metadata/common" xmlns:rpm="http://linux.duke.edu/metadata/rpm" packages="{n}">',
    ]
    filelists = ['<?xml version="1.0" encoding="UTF-8"?>', f'<filelists xmlns="http://linux.duke.edu/metadata/filelists" packages="{n}">']
    other = ['<?xml version="1.0" encoding="UTF-8"?>', f'<otherdata xmlns="http://linux.duke.edu/metadata/other" packages="{n}">']
    for p in pkgs:
        vendor = f"<rpm:vendor>{escape(p['vendor'])}</rpm:vendor>" if p["vendor"] else "<rpm:vendor/>"
        primary += [
            '<package type="rpm">',
            f"  <name>{escape(p['name'])}</name>",
            f"  <arch>{escape(p['arch'])}</arch>",
            f"  {ver_xml(p)}",
            f'  <checksum type="sha256" pkgid="YES">{p["pkgid"]}</checksum>',
            f"  <summary>{escape(p['summary'])}</summary>",
            f"  <description>{escape(p['description'])}</description>",
            f"  <packager>{escape(p['packager'])}</packager>",
            f"  <url>{escape(p['url'])}</url>",
            f'  <time file="{p["filetime"]}" build="{p["buildtime"]}"/>',
            f'  <size package="{p["size"]}" installed="{p["installed"]}" archive="{p["archive"]}"/>',
            f"  <location href={quoteattr(p['file'])}/>",
            "  <format>",
            f"    <rpm:license>{escape(p['license'])}</rpm:license>",
            f"    {vendor}",
            f"    <rpm:group>{escape(p['group'])}</rpm:group>",
            f"    <rpm:buildhost>{escape(p['buildhost'])}</rpm:buildhost>",
            f"    <rpm:sourcerpm>{escape(p['sourcerpm'])}</rpm:sourcerpm>",
            f'    <rpm:header-range start="{p["hstart"]}" end="{p["hend"]}"/>',
        ]
        if p["provides"]:
            primary += ["    <rpm:provides>", p["provides"], "    </rpm:provides>"]
        if p["requires"]:
            primary += ["    <rpm:requires>", p["requires"], "    </rpm:requires>"]
        primary += [file_xml(path, d, "    ") for path, d, _ in p["entries"] if primary_file(path)]
        primary += ["  </format>", "</package>"]
        head = f'<package pkgid="{p["pkgid"]}" name={quoteattr(p["name"])} arch={quoteattr(p["arch"])}>'
        filelists += [head, f"  {ver_xml(p)}"] + [file_xml(path, d, "  ") for path, d, _ in p["entries"]] + ["</package>"]
        other += [head, f"  {ver_xml(p)}", "</package>"]
    primary.append("</metadata>")
    filelists.append("</filelists>")
    other.append("</otherdata>")
    data = []
    for kind, lines in (("primary", primary), ("filelists", filelists), ("other", other)):
        xml = ("\n".join(lines) + "\n").encode("utf-8")
        comp = gz(xml)
        href = f"repodata/{sha(comp)}-{kind}.xml.gz"
        write(os.path.join(root, href), comp)
        data += [
            f'  <data type="{kind}">',
            f'    <checksum type="sha256">{sha(comp)}</checksum>',
            f'    <open-checksum type="sha256">{sha(xml)}</open-checksum>',
            f'    <location href="{href}"/>',
            f"    <timestamp>{epoch}</timestamp>",
            f"    <size>{len(comp)}</size>",
            f"    <open-size>{len(xml)}</open-size>",
            "  </data>",
        ]
    repomd = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        '<repomd xmlns="http://linux.duke.edu/metadata/repo" xmlns:rpm="http://linux.duke.edu/metadata/rpm">',
        f"  <revision>{epoch}</revision>",
        *data,
        "</repomd>",
    ]
    write(os.path.join(root, "repodata", "repomd.xml"), ("\n".join(repomd) + "\n").encode("utf-8"))
    base = base_url.rstrip("/")
    repo = [
        "[fil]",
        "name=fil",
        f"baseurl={base}/rpm",
        "enabled=1",
        "gpgcheck=1",
        "repo_gpgcheck=1",
        f"gpgkey={base}/rpm/fil-package-key.asc",
    ]
    write(os.path.join(root, "fil.repo"), ("\n".join(repo) + "\n").encode("utf-8"))


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    ap.add_argument("--files", required=True)
    ap.add_argument("--version", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--base-url", default="https://fil.anthonywertz.com")
    ap.add_argument("--epoch", type=int, default=int(time.time()))
    a = ap.parse_args()
    if not a.base_url.startswith("https://"):
        fail(f"--base-url must be an https:// URL: {a.base_url}")
    for sub in ("apt", "rpm"):
        if os.path.exists(os.path.join(a.out, sub)):
            shutil.rmtree(os.path.join(a.out, sub))
    os.makedirs(a.out, mode=0o755, exist_ok=True)
    apt_tree(a.files, a.version, a.out, a.epoch)
    dnf_tree(a.files, a.version, a.out, a.epoch, a.base_url)


if __name__ == "__main__":
    main()
