//! Which binary the client sends to a remote host (`docs/m4/plan.md` §3 U1, "Which binary is
//! sent"; `docs/m4/release.md` §4.3), and the checks it passes before a byte leaves:
//!
//! 1. with `--binary FILE`, that file;
//! 2. otherwise the bundled static Linux binary for the remote's OS and architecture, from
//!    `libexec/fil/remote/` beside this executable, checked against that directory's
//!    `SHA256SUMS`;
//! 3. otherwise this executable itself, only if it is a release build.
//!
//! Whatever is chosen must fit the remote (an ELF for its Linux architecture, a Mach-O for a
//! Mac) and pass the [`scan`]: no test hooks and no builder home paths. So a development build,
//! or a `cargo install` build that still carries its builder's paths, is never uploaded: it
//! would tell the remote host its builder's user name and home directory (DESIGN §11.1).
//! (The order puts an explicit `--binary` first; the plan lists it last, but a file the user
//! names on the command line is the one they mean.)

use std::path::{Path, PathBuf};

use super::layout::MAX_UPLOAD;
use super::remote::hex;

/// Whether this binary was marked as a release build when it was compiled: the release workflow
/// builds with `FIL_RELEASE_BUILD=1` (REL, `docs/m4/release.md`). A development or `cargo
/// install` build is not marked, and is never uploaded as itself.
pub const RELEASE_BUILD: bool = match option_env!("FIL_RELEASE_BUILD") {
    Some(v) => v.len() == 1 && v.as_bytes()[0] == b'1',
    None => false,
};

/// The remote's OS and architecture, as the bootstrap's greeting reports `uname -s` and
/// `uname -m`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remote {
    LinuxX86_64,
    LinuxAarch64,
    MacArm64,
    MacX86_64,
}

impl Remote {
    /// From `uname -s` and `uname -m`; `None` for anything fil does not build for.
    pub fn from_uname(sys: &str, machine: &str) -> Option<Remote> {
        match (sys, machine) {
            ("Linux", "x86_64") => Some(Remote::LinuxX86_64),
            ("Linux", "aarch64" | "arm64") => Some(Remote::LinuxAarch64),
            ("Darwin", "arm64") => Some(Remote::MacArm64),
            ("Darwin", "x86_64") => Some(Remote::MacX86_64),
            _ => None,
        }
    }

    /// The bundle's directory for this remote (`docs/m4/release.md` §4.3's table): the two Linux
    /// targets only. A Mac remote never maps to a Linux binary.
    pub fn bundle_dir(self) -> Option<&'static str> {
        match self {
            Remote::LinuxX86_64 => Some("x86_64-linux"),
            Remote::LinuxAarch64 => Some("aarch64-linux"),
            Remote::MacArm64 | Remote::MacX86_64 => None,
        }
    }

    /// `x86_64 Linux`, for messages and the log.
    pub fn describe(self) -> &'static str {
        match self {
            Remote::LinuxX86_64 => "x86_64 Linux",
            Remote::LinuxAarch64 => "aarch64 Linux",
            Remote::MacArm64 => "arm64 macOS",
            Remote::MacX86_64 => "x86_64 macOS",
        }
    }
}

/// The bundle's directory for a `uname -s` and `uname -m` pair (`docs/m4/release.md` §4.3).
pub fn bundle_dir_for(sys: &str, machine: &str) -> Option<&'static str> {
    Remote::from_uname(sys, machine).and_then(Remote::bundle_dir)
}

/// Whether the executable image `head` (its first bytes) runs on `remote`: an ELF of the
/// remote's machine for Linux; a thin Mach-O of its CPU, or a universal one that has it, for a
/// Mac.
pub fn fits(head: &[u8], remote: Remote) -> bool {
    const EM_X86_64: u16 = 62;
    const EM_AARCH64: u16 = 183;
    const CPU_X86_64: u32 = 0x0100_0007;
    const CPU_ARM64: u32 = 0x0100_000c;
    let u16le = |at: usize| {
        head.get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32le = |at: usize| {
        head.get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let u32be = |at: usize| {
        head.get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    match remote {
        Remote::LinuxX86_64 | Remote::LinuxAarch64 => {
            let want = if remote == Remote::LinuxX86_64 {
                EM_X86_64
            } else {
                EM_AARCH64
            };
            // ELF, 64-bit, little-endian.
            head.starts_with(b"\x7fELF\x02\x01") && u16le(18) == Some(want)
        }
        Remote::MacArm64 | Remote::MacX86_64 => {
            let want = if remote == Remote::MacArm64 {
                CPU_ARM64
            } else {
                CPU_X86_64
            };
            if u32le(0) == Some(0xfeed_facf) {
                return u32le(4) == Some(want);
            }
            if u32be(0) == Some(0xcafe_babe) {
                let n = u32be(4).unwrap_or(0).min(16) as usize;
                return (0..n).any(|i| u32be(8 + 20 * i) == Some(want));
            }
            false
        }
    }
}

/// Why a file is not uploaded.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("it is a development build (it has fil's test hooks in it)")]
    TestHooks,
    #[error(
        "it holds its builder's home directory paths (a development or `cargo install` build); upload a release binary with --binary"
    )]
    BuilderPaths,
    #[error("this fil is not a release build; name a release binary with --binary")]
    NotRelease,
    #[error("it is not a binary for the remote host ({0})")]
    WrongTarget(&'static str),
    #[error("it is larger than the 128 MiB the install accepts")]
    TooLarge,
    #[error("it does not match the bundle's SHA256SUMS")]
    BadBundle,
    #[error(
        "this machine has no fil binary for the remote host ({0}): name one with --binary, or install fil there by hand"
    )]
    NoBinary(&'static str),
    #[error("cannot read it: {0}")]
    Read(String),
}

/// A needle stored reversed, so the scanning binary does not contain what it scans for.
fn needle(reversed: &str) -> Vec<u8> {
    reversed.bytes().rev().collect()
}

/// The scan every uploaded file passes (`docs/m4/plan.md` §3 U1; REL's `release-check` makes the
/// same promise for release binaries): no test hooks (`FIL_TEST_`) and no builder home paths
/// (`/Users/`, `/home/`).
pub fn scan(bytes: &[u8]) -> Result<(), Refusal> {
    let has = |n: &[u8]| bytes.windows(n.len()).any(|w| w == n);
    if has(&needle("_TSET_LIF")) {
        return Err(Refusal::TestHooks);
    }
    if has(&needle("/sresU/")) || has(&needle("/emoh/")) {
        return Err(Refusal::BuilderPaths);
    }
    Ok(())
}

/// What will be sent: its bytes, their SHA-256 and where they came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub source: Source,
}

impl Source {
    /// The kind only, for the log: a `--binary` path is the user's own and stays out of it.
    pub fn kind(&self) -> &'static str {
        match self {
            Source::Named(_) => "named",
            Source::Bundle(_) => "bundle",
            Source::Itself => "itself",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// `--binary FILE`.
    Named(PathBuf),
    /// `libexec/fil/remote/<dir>/fil`.
    Bundle(&'static str),
    /// This executable.
    Itself,
}

fn sha256(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

fn read(path: &Path) -> Result<Vec<u8>, Refusal> {
    let meta = std::fs::metadata(path).map_err(|e| Refusal::Read(e.to_string()))?;
    if meta.len() > MAX_UPLOAD {
        return Err(Refusal::TooLarge);
    }
    let bytes = std::fs::read(path).map_err(|e| Refusal::Read(e.to_string()))?;
    // The bytes that would be sent, not what the metadata said a moment ago (a file can grow).
    if bytes.len() as u64 > MAX_UPLOAD {
        return Err(Refusal::TooLarge);
    }
    Ok(bytes)
}

/// `libexec/fil/remote` beside an executable at `exe` (canonical: `bin/fil`'s `../libexec`), so
/// the layout survives Homebrew's Cellar and its links.
pub fn bundle_root(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.parent()?.join("libexec/fil/remote"))
}

/// The hash `SHA256SUMS` lists for `rel` (`<64 hex>  <path>` lines, `docs/m4/release.md`
/// §4.2). A line that does not parse is ignored; a path listed twice is refused.
pub fn listed_hash(sums: &str, rel: &str) -> Option<String> {
    let mut found = None;
    for line in sums.lines() {
        let Some((hash, path)) = line.split_once("  ") else {
            continue;
        };
        let hex_ok =
            hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if hex_ok && path == rel {
            if found.is_some() {
                return None;
            }
            found = Some(hash.to_string());
        }
    }
    found
}

/// Chooses and checks the binary for `remote`. `named` is `--binary`, `exe` this executable's
/// canonical path, `release` whether it is a marked release build ([`RELEASE_BUILD`]).
pub fn choose(
    remote: Remote,
    named: Option<&Path>,
    exe: &Path,
    release: bool,
) -> Result<Upload, Refusal> {
    let check = |bytes: Vec<u8>, source: Source| {
        if !fits(&bytes, remote) {
            return Err(Refusal::WrongTarget(remote.describe()));
        }
        scan(&bytes)?;
        Ok(Upload {
            sha256: sha256(&bytes),
            bytes,
            source,
        })
    };
    if let Some(path) = named {
        return check(read(path)?, Source::Named(path.to_path_buf()));
    }
    if let (Some(dir), Some(root)) = (remote.bundle_dir(), bundle_root(exe)) {
        let rel = format!("{dir}/fil");
        let file = root.join(&rel);
        if file.is_file() {
            let sums = std::fs::read_to_string(root.join("SHA256SUMS"))
                .map_err(|e| Refusal::Read(e.to_string()))?;
            let bytes = read(&file)?;
            if listed_hash(&sums, &rel) != Some(sha256(&bytes)) {
                return Err(Refusal::BadBundle);
            }
            return check(bytes, Source::Bundle(dir));
        }
    }
    // A build that is not marked as a release is never sent as itself, whatever its size or
    // target: said first, and without reading the file (a development build with its debug
    // information is larger than the cap on Linux).
    if !release {
        return Err(Refusal::NotRelease);
    }
    let bytes = read(exe)?;
    if !fits(&bytes, remote) {
        return Err(Refusal::NoBinary(remote.describe()));
    }
    check(bytes, Source::Itself)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The first bytes of a 64-bit little-endian ELF for `machine`, padded.
    fn elf(machine: u16) -> Vec<u8> {
        let mut v = b"\x7fELF\x02\x01\x01\0".to_vec();
        v.resize(18, 0);
        v.extend_from_slice(&machine.to_le_bytes());
        v.resize(256, 0);
        v
    }

    fn macho(cpu: u32) -> Vec<u8> {
        let mut v = 0xfeed_facfu32.to_le_bytes().to_vec();
        v.extend_from_slice(&cpu.to_le_bytes());
        v.resize(256, 0);
        v
    }

    fn fat(cpus: &[u32]) -> Vec<u8> {
        let mut v = 0xcafe_babeu32.to_be_bytes().to_vec();
        v.extend_from_slice(&(cpus.len() as u32).to_be_bytes());
        for c in cpus {
            v.extend_from_slice(&c.to_be_bytes());
            v.extend_from_slice(&[0; 16]);
        }
        v.resize(256, 0);
        v
    }

    #[test]
    fn the_bundle_table_maps_linux_only() {
        assert_eq!(bundle_dir_for("Linux", "x86_64"), Some("x86_64-linux"));
        assert_eq!(bundle_dir_for("Linux", "aarch64"), Some("aarch64-linux"));
        assert_eq!(bundle_dir_for("Linux", "arm64"), Some("aarch64-linux"));
        for (s, m) in [
            ("Darwin", "arm64"),
            ("Darwin", "x86_64"),
            ("Darwin", "aarch64"),
            ("Linux", "armv7l"),
            ("Linux", "i686"),
            ("FreeBSD", "amd64"),
            ("linux", "x86_64"),
            ("", ""),
        ] {
            assert_eq!(bundle_dir_for(s, m), None, "{s} {m}");
        }
    }

    #[test]
    fn a_binary_must_fit_the_remote() {
        assert!(fits(&elf(62), Remote::LinuxX86_64));
        assert!(!fits(&elf(62), Remote::LinuxAarch64));
        assert!(fits(&elf(183), Remote::LinuxAarch64));
        assert!(!fits(&elf(183), Remote::MacArm64));
        assert!(fits(&macho(0x0100_000c), Remote::MacArm64));
        assert!(!fits(&macho(0x0100_000c), Remote::MacX86_64));
        let universal = fat(&[0x0100_0007, 0x0100_000c]);
        assert!(fits(&universal, Remote::MacArm64) && fits(&universal, Remote::MacX86_64));
        assert!(!fits(&universal, Remote::LinuxX86_64));
        assert!(!fits(b"#!/bin/sh\n", Remote::LinuxX86_64));
        assert!(!fits(b"", Remote::MacArm64));
    }

    #[test]
    fn the_scan_refuses_test_hooks_and_builder_paths() {
        let mut clean = elf(62);
        clean.extend_from_slice(b"/rustc/abc/library/std/src/io.rs fil server --stdio");
        assert_eq!(scan(&clean), Ok(()));
        for (dirt, why) in [
            (&b"FIL_TEST_SOCKET_DIR"[..], Refusal::TestHooks),
            (
                b"/Users/someone/.cargo/registry/src/x.rs",
                Refusal::BuilderPaths,
            ),
            (b"/home/runner/work/fil/src/main.rs", Refusal::BuilderPaths),
        ] {
            let mut b = clean.clone();
            b.extend_from_slice(dirt);
            assert_eq!(scan(&b), Err(why));
        }
    }

    /// An unpacked release layout in a scratch directory: `bin/fil` and the bundle.
    struct Layout {
        root: PathBuf,
    }

    impl Layout {
        fn new(name: &str, bundled: &[u8], listed: &[u8]) -> Layout {
            let root =
                std::env::temp_dir().join(format!("fil-bundle-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let remote = root.join("libexec/fil/remote");
            std::fs::create_dir_all(root.join("bin")).unwrap();
            std::fs::create_dir_all(remote.join("x86_64-linux")).unwrap();
            std::fs::write(root.join("bin/fil"), elf(183)).unwrap();
            std::fs::set_permissions(root.join("bin/fil"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            std::fs::write(remote.join("x86_64-linux/fil"), bundled).unwrap();
            std::fs::write(
                remote.join("SHA256SUMS"),
                format!(
                    "{}  aarch64-linux/fil\n{}  x86_64-linux/fil\n",
                    "0".repeat(64),
                    sha256(listed)
                ),
            )
            .unwrap();
            Layout { root }
        }
        fn exe(&self) -> PathBuf {
            self.root.join("bin/fil")
        }
    }

    impl Drop for Layout {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn the_bundle_is_chosen_and_checked_before_anything_is_sent() {
        let good = elf(62);
        let l = Layout::new("good", &good, &good);
        let up = choose(Remote::LinuxX86_64, None, &l.exe(), false).unwrap();
        assert_eq!(up.source, Source::Bundle("x86_64-linux"));
        assert_eq!(up.sha256, sha256(&good));
        assert_eq!(up.bytes, good);

        // One byte changed: refused, whatever else is on offer.
        let mut tampered = good.clone();
        tampered[100] ^= 1;
        let l = Layout::new("tampered", &tampered, &good);
        assert_eq!(
            choose(Remote::LinuxX86_64, None, &l.exe(), true),
            Err(Refusal::BadBundle)
        );

        // No bundle entry for this remote: this executable (an aarch64 ELF here), only as a
        // release build, and only for a remote it runs on.
        let l = Layout::new("self", &good, &good);
        assert_eq!(
            choose(Remote::LinuxAarch64, None, &l.exe(), false),
            Err(Refusal::NotRelease)
        );
        assert_eq!(
            choose(Remote::LinuxAarch64, None, &l.exe(), true)
                .unwrap()
                .source,
            Source::Itself
        );
        assert!(matches!(
            choose(Remote::MacArm64, None, &l.exe(), true),
            Err(Refusal::NoBinary(_))
        ));
        // Not a release: said before the file is read, whatever its size.
        let big = l.exe().with_file_name("big-fil");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_UPLOAD + 1)
            .unwrap();
        assert_eq!(
            choose(Remote::LinuxAarch64, None, &big, false),
            Err(Refusal::NotRelease)
        );
        let _ = std::fs::remove_file(&big);
    }

    #[test]
    fn a_named_binary_must_fit_and_pass_the_scan() {
        let l = Layout::new("named", &elf(62), &elf(62));
        let named = l.root.join("other");
        let mut dev = elf(62);
        dev.extend_from_slice(b"/home/builder/.cargo/x");
        std::fs::write(&named, &dev).unwrap();
        assert_eq!(
            choose(Remote::LinuxX86_64, Some(&named), &l.exe(), true),
            Err(Refusal::BuilderPaths)
        );
        std::fs::write(&named, elf(183)).unwrap();
        assert_eq!(
            choose(Remote::LinuxX86_64, Some(&named), &l.exe(), true),
            Err(Refusal::WrongTarget("x86_64 Linux"))
        );
        std::fs::write(&named, elf(62)).unwrap();
        assert_eq!(
            choose(Remote::LinuxX86_64, Some(&named), &l.exe(), false)
                .unwrap()
                .source,
            Source::Named(named.clone())
        );
    }

    /// This test binary is a development build: it is never uploaded as itself.
    #[test]
    fn a_development_build_is_never_eligible() {
        assert!(!RELEASE_BUILD || std::env::var_os("FIL_RELEASE_BUILD").is_some());
        let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert!(
            scan(&me).is_err(),
            "a test binary carries the builder's paths or the hooks"
        );
    }

    #[test]
    fn more_than_128_mib_is_refused_before_it_is_read() {
        let l = Layout::new("big", &elf(62), &elf(62));
        let big = l.root.join("big");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_UPLOAD + 1).unwrap(); // sparse: no 128 MiB written
        assert_eq!(
            choose(Remote::LinuxX86_64, Some(&big), &l.exe(), true),
            Err(Refusal::TooLarge)
        );
        f.set_len(MAX_UPLOAD).unwrap();
        assert_eq!(
            choose(Remote::LinuxX86_64, Some(&big), &l.exe(), true),
            Err(Refusal::WrongTarget("x86_64 Linux")),
            "exactly the cap is read (and then is not a binary)"
        );
    }

    #[test]
    fn sums_lines_are_parsed_strictly() {
        let h = "a".repeat(64);
        assert_eq!(
            listed_hash(&format!("{h}  x86_64-linux/fil\n"), "x86_64-linux/fil"),
            Some(h.clone())
        );
        assert_eq!(
            listed_hash(&format!("{h} x86_64-linux/fil\n"), "x86_64-linux/fil"),
            None
        );
        assert_eq!(
            listed_hash(&format!("{h}  ./x86_64-linux/fil\n"), "x86_64-linux/fil"),
            None
        );
        assert_eq!(
            listed_hash(
                &format!("{h}  x86_64-linux/fil\n{h}  x86_64-linux/fil\n"),
                "x86_64-linux/fil"
            ),
            None,
            "listed twice"
        );
        assert_eq!(
            listed_hash(
                &format!("{}  x86_64-linux/fil\n", h.to_uppercase()),
                "x86_64-linux/fil"
            ),
            None
        );
    }
}
