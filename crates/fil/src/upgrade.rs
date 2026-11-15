//! How this `fil` was installed, and the command that upgrades it (1.0 decision 6, DESIGN.md
//! section 14): the second line of `fil --version`.
//!
//! fil never updates itself and never looks for a new version (DESIGN.md section 11.1.1: no
//! downloads at run time, no update check of any kind). It only says how the user would upgrade
//! it, from what is already on this machine: where its own executable is, and the package
//! databases' plain files that name it. Nothing here touches the network, runs a program (no
//! `dpkg`, `rpm`, `brew` or `pacman`), or writes anything. Every look is a `stat`, a directory
//! listing or the read of a small file, and a look that fails is "not this way".
//!
//! [`detect`] decides from facts alone ([`Facts`]), so the tests can give it any machine;
//! [`upgrade_line`] is what `--version` prints.
//!
//! Where a host's daemon is newer than this client, `fil ls` says so ([`newer_daemon`]): the
//! daemon names its version in its `Hello` (the free-form `agent`, "fil-daemon X.Y.Z"), which the
//! client has anyway. Nothing is asked of the host for it.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

/// Where the release files are, for the ways that upgrade by downloading one.
pub const RELEASES: &str = "https://github.com/anthonytw/fil/releases/latest";

/// How this `fil` came to be where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// In a Homebrew Cellar (`.../Cellar/fil/<version>/bin/fil`), from the tap.
    Homebrew,
    /// `cargo install`: in `$CARGO_INSTALL_ROOT/bin`, `$CARGO_HOME/bin` or `~/.cargo/bin`.
    Cargo,
    /// In the Nix store.
    Nix,
    /// The `.deb`: `/usr/bin/fil`, listed by dpkg's file list of the package `fil`.
    Deb,
    /// The AUR package `fil-shell-bin`: `/usr/bin/fil`, with that package in pacman's database.
    Aur,
    /// `/usr/bin/fil` on a host with an rpm database and neither of the two above: the `.rpm`, as
    /// far as can be told without reading that database (it is not a plain file).
    Rpm,
    /// A copy `fil install` (or a first attach) put in `~/.local/share/fil/<ver>-w<wire>/` on this
    /// host: a client of a newer version installs its own copy beside it.
    FilInstall,
    /// `~/.local/bin/fil`: where the install script puts it, and where the docs say to copy the
    /// archive's `bin/fil`.
    LocalBin,
    /// Anywhere else: copied by hand from a release archive.
    Manual(PathBuf),
}

/// What [`detect`] needs to know about this machine. [`System`] is the real one.
pub trait Facts {
    /// This executable, with every symbolic link resolved.
    fn exe(&self) -> Option<PathBuf>;
    /// `$HOME`, when it is absolute.
    fn home(&self) -> Option<PathBuf>;
    /// The value of an environment variable, when it is an absolute path.
    fn env_path(&self, name: &str) -> Option<PathBuf>;
    /// The contents of a small file, or `None` (absent, unreadable, too big).
    fn read(&self, path: &Path) -> Option<String>;
    /// Whether a path exists.
    fn exists(&self, path: &Path) -> bool;
    /// A path with every symbolic link resolved, if it exists.
    fn canonical(&self, path: &Path) -> Option<PathBuf>;
    /// The names in a directory, or nothing.
    fn list(&self, dir: &Path) -> Vec<String>;
}

/// The facts of this machine.
pub struct System;

/// The most read from one file: dpkg's list for fil is a few dozen lines, pacman's `files` too.
const MAX_READ: u64 = 1 << 20;

impl Facts for System {
    fn exe(&self) -> Option<PathBuf> {
        std::env::current_exe()
            .ok()
            .and_then(|p| std::fs::canonicalize(p).ok())
    }

    fn home(&self) -> Option<PathBuf> {
        fil_daemon::paths::home_dir()
    }

    fn env_path(&self, name: &str) -> Option<PathBuf> {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    }

    fn read(&self, path: &Path) -> Option<String> {
        use std::io::Read;
        let f = std::fs::File::open(path).ok()?;
        let mut s = String::new();
        f.take(MAX_READ).read_to_string(&mut s).ok()?;
        Some(s)
    }

    fn exists(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }

    fn canonical(&self, path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }

    fn list(&self, dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Where the distribution packages put the binary.
const USR_BIN_FIL: &str = "/usr/bin/fil";

/// How the `fil` at [`Facts::exe`] was installed.
pub fn detect(f: &dyn Facts) -> Install {
    let Some(exe) = f.exe() else {
        return Install::Manual(PathBuf::from("fil"));
    };
    let s = exe.to_string_lossy();
    if s.starts_with("/nix/store/") {
        return Install::Nix;
    }
    if s.contains("/Cellar/fil/") {
        return Install::Homebrew;
    }
    let dir = exe.parent();
    let home = f.home();
    let same = |a: Option<&Path>, b: Option<PathBuf>| match (a, b) {
        (Some(a), Some(b)) => a == b || f.canonical(&b).is_some_and(|b| a == b),
        _ => false,
    };
    let cargo_roots = [
        f.env_path("CARGO_INSTALL_ROOT").map(|r| r.join("bin")),
        f.env_path("CARGO_HOME").map(|r| r.join("bin")),
        home.as_ref().map(|h| h.join(".cargo/bin")),
    ];
    if cargo_roots.into_iter().any(|r| same(dir, r)) {
        return Install::Cargo;
    }
    if let Some(home) = &home {
        let root = crate::install::layout::install_root(home);
        let in_version_dir = dir.is_some_and(|d| {
            same(d.parent(), Some(root.clone()))
                && d.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(crate::install::layout::parse_version_dir)
                    .is_some()
        });
        if in_version_dir {
            return Install::FilInstall;
        }
        if same(Some(&exe), Some(home.join(".local/bin/fil"))) {
            return Install::LocalBin;
        }
    }
    if exe == Path::new(USR_BIN_FIL) {
        if dpkg_lists_fil(f) {
            return Install::Deb;
        }
        if pacman_has_fil(f) {
            return Install::Aur;
        }
        if ["/usr/lib/sysimage/rpm", "/var/lib/rpm"]
            .iter()
            .any(|d| f.exists(Path::new(d)))
        {
            return Install::Rpm;
        }
    }
    Install::Manual(exe)
}

/// dpkg's file list of the package `fil` (`fil.list`, or `fil:<arch>.list` for a multi-arch
/// one) has the line `/usr/bin/fil`.
fn dpkg_lists_fil(f: &dyn Facts) -> bool {
    let info = Path::new("/var/lib/dpkg/info");
    f.list(info)
        .iter()
        .filter(|n| *n == "fil.list" || (n.starts_with("fil:") && n.ends_with(".list")))
        .any(|n| {
            f.read(&info.join(n))
                .is_some_and(|t| t.lines().any(|l| l == USR_BIN_FIL))
        })
}

/// pacman's database has the package `fil-shell-bin` (`local/fil-shell-bin-<ver>-<rel>/`), whose
/// `files` lists `usr/bin/fil`.
fn pacman_has_fil(f: &dyn Facts) -> bool {
    let local = Path::new("/var/lib/pacman/local");
    f.list(local)
        .iter()
        .filter(|n| n.starts_with("fil-shell-bin-"))
        .any(|n| {
            f.read(&local.join(n).join("files"))
                .is_some_and(|t| t.lines().any(|l| l == "usr/bin/fil"))
        })
}

/// The line `fil --version` prints after its first: how this copy was installed and how to
/// upgrade it. Plain text, no newline.
pub fn upgrade_line(install: &Install) -> String {
    match install {
        Install::Homebrew => {
            "Installed with Homebrew. Upgrade: brew upgrade anthonytw/tap/fil".to_string()
        }
        Install::Cargo => {
            "Installed with cargo. Upgrade: cargo install --locked fil-shell".to_string()
        }
        Install::Nix => "Installed with Nix. Upgrade: nix profile upgrade fil (a profile installed \
             from a release tag: nix profile install github:anthonytw/fil/vX.Y.Z), or your \
             flake's or configuration's input"
            .to_string(),
        Install::Deb => format!(
            "Installed from the .deb. Upgrade: sudo apt install ./fil_X.Y.Z_ARCH.deb, from {RELEASES}"
        ),
        Install::Aur => {
            "Installed from the AUR (fil-shell-bin). Upgrade: with your AUR helper, or makepkg -si \
             in its updated clone"
                .to_string()
        }
        Install::Rpm => format!(
            "Installed in /usr/bin, by the .rpm as far as fil can tell. Upgrade: sudo dnf install \
             ./fil-X.Y.Z-1.ARCH.rpm, from {RELEASES}"
        ),
        Install::FilInstall => "Installed by fil install (or a first attach) on this host. \
             Upgrade: connect to it with a newer fil, which installs its own version beside this one"
            .to_string(),
        Install::LocalBin => format!(
            "Installed in ~/.local/bin. Upgrade: run the install script again, or copy bin/fil \
             from the new release's archive over it ({RELEASES})"
        ),
        Install::Manual(path) => format!(
            "Installed by hand at {}. Upgrade: copy bin/fil from the new release's archive over \
             it ({RELEASES})",
            fil_proto::text::sanitize_inline(path.as_os_str().as_encoded_bytes())
        ),
    }
}

/// A release version, as far as ordering goes: `MAJOR.MINOR.PATCH`, and an optional pre-release
/// after `-`, which sorts before the release (semver's rule, with the pre-release compared as a
/// whole string, which is enough for fil's `rc.N`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Option<String>,
}

impl Version {
    /// `None` for anything that is not such a version.
    pub fn parse(s: &str) -> Option<Version> {
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) if !p.is_empty() => (c, Some(p.to_string())),
            Some(_) => return None,
            None => (s, None),
        };
        let mut parts = core.split('.');
        let mut n = [0u64; 3];
        for slot in &mut n {
            let p = parts.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.len() > 9 {
                return None;
            }
            *slot = p.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        if pre.as_deref().is_some_and(|p| {
            !p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        }) {
            return None;
        }
        Some(Version { core: n, pre })
    }

    /// This build's version.
    pub fn ours() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("the package version is a version")
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.core[0], self.core[1], self.core[2])?;
        if let Some(p) = &self.pre {
            write!(f, "-{p}")?;
        }
        Ok(())
    }
}

/// The daemon's version from the `agent` of its `Hello` (`fil-daemon X.Y.Z`), when it is newer
/// than `ours`. The agent is free-form and untrusted: anything that is not exactly that shape is
/// ignored, and what is returned is a parsed version, not the daemon's text.
pub fn newer_daemon(agent: &str, ours: &Version) -> Option<Version> {
    let v = Version::parse(agent.strip_prefix("fil-daemon ")?)?;
    (v > *ours).then_some(v)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    #[derive(Default)]
    struct Fake {
        exe: Option<PathBuf>,
        home: Option<PathBuf>,
        env: HashMap<&'static str, PathBuf>,
        files: HashMap<PathBuf, String>,
        dirs: HashSet<PathBuf>,
    }

    impl Fake {
        fn at(exe: &str) -> Fake {
            Fake {
                exe: Some(exe.into()),
                home: Some("/home/u".into()),
                ..Fake::default()
            }
        }
        fn file(mut self, path: &str, text: &str) -> Fake {
            self.files.insert(path.into(), text.into());
            self
        }
        fn dir(mut self, path: &str) -> Fake {
            self.dirs.insert(path.into());
            self
        }
        fn env(mut self, name: &'static str, path: &str) -> Fake {
            self.env.insert(name, path.into());
            self
        }
    }

    impl Facts for Fake {
        fn exe(&self) -> Option<PathBuf> {
            self.exe.clone()
        }
        fn home(&self) -> Option<PathBuf> {
            self.home.clone()
        }
        fn env_path(&self, name: &str) -> Option<PathBuf> {
            self.env.get(name).cloned()
        }
        fn read(&self, path: &Path) -> Option<String> {
            self.files.get(path).cloned()
        }
        fn exists(&self, path: &Path) -> bool {
            self.dirs.contains(path) || self.files.contains_key(path)
        }
        fn canonical(&self, _path: &Path) -> Option<PathBuf> {
            None
        }
        fn list(&self, dir: &Path) -> Vec<String> {
            // Every entry whose path, or a parent of it, is right below `dir`.
            let mut names: Vec<String> = self
                .files
                .keys()
                .chain(self.dirs.iter())
                .flat_map(|p| p.ancestors())
                .filter(|p| p.parent() == Some(dir))
                .filter_map(|p| p.file_name()?.to_str().map(String::from))
                .collect();
            names.sort();
            names.dedup();
            names
        }
    }

    #[test]
    fn each_way_is_told_from_the_path_and_the_package_files() {
        let cases: Vec<(Fake, Install)> = vec![
            (
                Fake::at("/opt/homebrew/Cellar/fil/0.4.0/bin/fil"),
                Install::Homebrew,
            ),
            (
                Fake::at("/home/linuxbrew/.linuxbrew/Cellar/fil/0.4.0/bin/fil"),
                Install::Homebrew,
            ),
            (Fake::at("/home/u/.cargo/bin/fil"), Install::Cargo),
            (
                Fake::at("/opt/cargo/bin/fil").env("CARGO_HOME", "/opt/cargo"),
                Install::Cargo,
            ),
            (
                Fake::at("/srv/tools/bin/fil").env("CARGO_INSTALL_ROOT", "/srv/tools"),
                Install::Cargo,
            ),
            (
                Fake::at("/nix/store/abc123-fil-shell-0.4.0/bin/fil"),
                Install::Nix,
            ),
            (
                Fake::at("/usr/bin/fil").file(
                    "/var/lib/dpkg/info/fil.list",
                    "/.\n/usr\n/usr/bin\n/usr/bin/fil\n",
                ),
                Install::Deb,
            ),
            (
                Fake::at("/usr/bin/fil")
                    .file("/var/lib/dpkg/info/fil:amd64.list", "/usr/bin/fil\n"),
                Install::Deb,
            ),
            (
                Fake::at("/usr/bin/fil").file(
                    "/var/lib/pacman/local/fil-shell-bin-0.4.0-1/files",
                    "%FILES%\nusr/\nusr/bin/\nusr/bin/fil\n",
                ),
                Install::Aur,
            ),
            (
                Fake::at("/usr/bin/fil").dir("/usr/lib/sysimage/rpm"),
                Install::Rpm,
            ),
            (Fake::at("/usr/bin/fil").dir("/var/lib/rpm"), Install::Rpm),
            (
                Fake::at("/home/u/.local/share/fil/0.4.0-w1.16/fil"),
                Install::FilInstall,
            ),
            (Fake::at("/home/u/.local/bin/fil"), Install::LocalBin),
            (
                Fake::at("/usr/local/bin/fil"),
                Install::Manual("/usr/local/bin/fil".into()),
            ),
        ];
        for (facts, want) in cases {
            let exe = facts.exe.clone();
            assert_eq!(detect(&facts), want, "{exe:?}");
        }
    }

    #[test]
    fn a_near_miss_is_not_taken_for_a_package() {
        // /usr/bin/fil that dpkg's list of another package names, or fil's list without it.
        let other = Fake::at("/usr/bin/fil")
            .file("/var/lib/dpkg/info/fil-tools.list", "/usr/bin/fil\n")
            .file("/var/lib/dpkg/info/fil.list", "/usr/bin/filx\n");
        assert_eq!(detect(&other), Install::Manual("/usr/bin/fil".into()));
        // Another AUR package, or fil-shell-bin without the binary.
        let aur = Fake::at("/usr/bin/fil")
            .file("/var/lib/pacman/local/fil-shell-1/files", "usr/bin/fil\n")
            .file(
                "/var/lib/pacman/local/fil-shell-bin-1/files",
                "usr/bin/other\n",
            );
        assert_eq!(detect(&aur), Install::Manual("/usr/bin/fil".into()));
        // A dpkg list is only read for /usr/bin/fil.
        let elsewhere =
            Fake::at("/usr/local/bin/fil").file("/var/lib/dpkg/info/fil.list", "/usr/bin/fil\n");
        assert_eq!(
            detect(&elsewhere),
            Install::Manual("/usr/local/bin/fil".into())
        );
        // Not a version directory, or not under the install root.
        assert_eq!(
            detect(&Fake::at("/home/u/.local/share/fil/latest/fil")),
            Install::Manual("/home/u/.local/share/fil/latest/fil".into())
        );
        assert_eq!(
            detect(&Fake::at("/tmp/0.4.0-w1.16/fil")),
            Install::Manual("/tmp/0.4.0-w1.16/fil".into())
        );
        // No executable path at all.
        let none = Fake::default();
        assert_eq!(detect(&none), Install::Manual("fil".into()));
    }

    #[test]
    fn every_way_names_a_command_or_a_place() {
        for i in [
            Install::Homebrew,
            Install::Cargo,
            Install::Nix,
            Install::Deb,
            Install::Aur,
            Install::Rpm,
            Install::FilInstall,
            Install::LocalBin,
            Install::Manual("/opt/x/fil".into()),
        ] {
            let line = upgrade_line(&i);
            assert!(line.contains("Upgrade: "), "{line}");
            assert!(!line.contains('\n'), "{line}");
        }
        assert!(upgrade_line(&Install::Homebrew).ends_with("brew upgrade anthonytw/tap/fil"));
        assert!(upgrade_line(&Install::Cargo).ends_with("cargo install --locked fil-shell"));
        assert!(upgrade_line(&Install::Manual("/opt/x/fil".into())).contains("/opt/x/fil"));
        // A path is shown sanitized.
        let odd = upgrade_line(&Install::Manual("/tmp/\x1b[2Jfil".into()));
        assert!(!odd.contains('\x1b'), "{odd:?}");
    }

    #[test]
    fn versions_order_as_releases_do() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("0.4.0") > v("0.3.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.10.0") > v("0.9.0"), "numbers, not text");
        assert!(v("1.0.0") > v("1.0.0-rc.1"), "a pre-release comes first");
        assert!(v("1.0.0-rc.2") > v("1.0.0-rc.1"));
        assert_eq!(v("0.4.0").to_string(), "0.4.0");
        assert_eq!(v("1.0.0-rc.1").to_string(), "1.0.0-rc.1");
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "1.2.3-",
            "v1.2.3",
            "1.2.3-\x1b",
            "1..3",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Version::ours().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn a_newer_daemon_is_read_from_its_hello_only_in_the_known_shape() {
        let ours = Version::parse("0.4.0").unwrap();
        assert_eq!(
            newer_daemon("fil-daemon 0.4.1", &ours),
            Version::parse("0.4.1")
        );
        assert_eq!(newer_daemon("fil-daemon 0.4.0", &ours), None);
        assert_eq!(newer_daemon("fil-daemon 0.3.9", &ours), None);
        assert_eq!(newer_daemon("fil-relay 9.9.9", &ours), None);
        assert_eq!(newer_daemon("fil-daemon 9.9.9 \x1b[2J", &ours), None);
        assert_eq!(newer_daemon("test-daemon", &ours), None);
    }
}
