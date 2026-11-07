# The release contract

> **Status: written by M4's package R0 (2026-11-04) from `docs/m4/plan.md` §2.** This is the reference that the release workflow (REL), the installer (U1), the packaging (PKG), the notices (LIC) and the docs (DOC) build on and cite. `docs/m4/plan.md` says why; this file says what. If the two disagree, this file wins for the values below, and the plan is corrected. Items marked **proposed** are the coordinator's calls, which review and the owner may change. Items marked **FROZEN** are a contract between versions of fil: once a release carries them, they change only by the rule in "Changing a frozen item". Everything about the remote install layout and the `--install-gc` contract rested on the owner's answer to M4 OQ-1, which is yes (owner, 2026-11-04).
>
> `DESIGN.md` stays the source of truth. A package that changes a decision here updates this file and `DESIGN.md` in the same PR.

Examples use package version `0.1.0` and wire `1.12`. The wire on `main` at the time of writing is `1.11`; the release carries whatever `PROTOCOL_MAJOR`.`PROTOCOL_REVISION` is on its tag.

## 1. Versioning and the wire

- **The package version** is SemVer, in `[workspace.package] version` in the root `Cargo.toml`. It is the one version of every crate. The published crates also pin each other at exactly that version (`=X.Y.Z` in `[workspace.dependencies]`, `docs/m4/packaging.md` §3), so a bump changes those lines and `Cargo.lock` too: `just set-version X.Y.Z` does all of it (cargo refuses a tree where they disagree). A release version is `X.Y.Z` or `X.Y.Z-PRE` with PRE of `[0-9A-Za-z.]`, which the .deb and .rpm need (nfpm's semver; `release-check` refuses another). The first release is **0.1.0**, an ssh/et drop-in without prediction (decided, owner 2026-11-04, 10:28 ET: N9; it has no `predict` key and no `--predict` flag, and prediction is 0.2.0's headline), then 1.0.0 "after real use" (decided, owner 2026-11-04: the option "0.1.0 first, 1.0.0 after real use"); one or two 0.x releases in between, and 1.0 once the config and flags have met real use, is the recommendation they accepted. Under SemVer, 0.x means the config, flags and exit statuses may still change.
- **The wire** is `PROTOCOL_MAJOR`.`PROTOCOL_REVISION` (`crates/fil-proto/src/limits.rs`). It is independent of the package version. Two builds talk only when their wires are equal (`DESIGN.md` §5.3, §8.5).
- **A patch release never changes the wire.** REL's `release-check` compares the revision on the last line of the golden's `[history]` with that of the previous tag of the same minor.
- **A release that changes the wire bumps the minor version** (0.1 to 0.2 before 1.0, 1.2 to 1.3 after). Its release notes say "upgrade fil on both ends". Daemons of different wires coexist on a host, so this is an upgrade, not a breakage.
- **A change of `PROTOCOL_MAJOR`** changes the frozen opening (`CLAUDE.md`, "The wire has a revision"). After 1.0 it bumps the package's major.
- **Patch releases and running daemons.** A patch keeps the wire, so its relay finds an older daemon of the same wire through the same socket name and uses it. A patch's daemon-side fixes take effect only after that daemon exits (idle, with no sessions left). The notes of a patch with daemon-side fixes say "end your sessions (or `fil kill` them) on each host to pick up these fixes". Showing the daemon's package version in `fil ls` needs a wire field and is deferred.
- **What 1.0 means** is decided at 1.0, not in M4.
- **`rust-version`** in the root `Cargo.toml` is the pinned toolchain, because `cargo install --git` does not read `rust-toolchain.toml`.
- **The tag** is `v<ver>` (for example `v0.1.0`): annotated and SSH-signed, on `main`. `release-check` requires that `<ver>` equals the version in `Cargo.toml` and that `CHANGELOG.md` has a section for it.

## 2. The version string

`fil --version` prints, as its first line:

```
fil 0.1.0 (wire 1.12)
```

- **Format:** `fil <ver> (wire <major>.<rev>)`, where `<ver>` is `CARGO_PKG_VERSION` and `<major>.<rev>` is the wire of the build (`crates/fil/src/cli.rs`, `version_string`). Nothing in it depends on when or where the build was made.
- **FROZEN: the first line.** Tests, the bootstrap script (which greps the whole `--version` output of a candidate for `(wire <major>.<rev>)`) and the docs parse it. LIC may add GNU-style notice lines after it and nothing before it, and **no later line may contain the text `(wire `**, so the script's match can only come from the first line.
- **`<ver>`** starts with a digit, is at most 64 characters, and uses only `[0-9A-Za-z.-]`. A pre-release is `0.1.0-rc.1`. Build metadata (`+…`) is not used, and a `+` is refused by the compile-time check of the remote command (§5.2). This is exactly the version part of the install directory's name pattern (§6.2).

## 3. Targets

| Target | Artifact | Role |
|---|---|---|
| `x86_64-unknown-linux-musl` | static | client and remote |
| `aarch64-unknown-linux-musl` | static | client and remote (Raspberry Pi 4 and later, ARM servers) |
| `aarch64-apple-darwin` and `x86_64-apple-darwin` | **one universal binary** (`lipo`) | client and remote |

- **One macOS file** keeps Homebrew simple, and a Mac can upload itself to a Mac of either architecture. It is about twice the size of one architecture.
- **The Intel half has a limited test life.** GitHub is retiring its Intel macOS images, and Rust moved `x86_64-apple-darwin` to tier 2 in 2025. MAC tests Intel for as long as a hosted Intel image exists. After that the Intel half is built but not tested, and the release notes say so.
- **Not v1 targets:** Windows (decided, `DESIGN.md` §2; WSL counts as Linux), 32-bit ARM, FreeBSD and the other BSDs. Nothing stops a user from building them.

## 4. Release artifacts

### 4.1 The files

Each release carries exactly these files (the `.ci` archive is the coordinator's decision of 2026-11-06, made after the review of the macOS signing found that CI's ad hoc signature cannot be reproduced on another Mac, §10.3):

| File | What |
|---|---|
| `fil-<ver>-x86_64-linux-musl.tar.gz` | the Linux x86_64 archive |
| `fil-<ver>-aarch64-linux-musl.tar.gz` | the Linux aarch64 archive |
| `fil-<ver>-universal-apple-darwin.tar.gz` | the macOS archive. The owner replaces it by one whose `bin/fil` is Developer-ID signed and notarized (§10.3) |
| `fil-<ver>-universal-apple-darwin.ci.tar.gz` | the macOS archive exactly as CI built and attested it, ad hoc signed, never replaced (decided, coordinator, 2026-11-06; §10.3). Before signing it is the same file as the macOS archive above. The `.ci` before `.tar.gz` keeps it inside the `fil-*.tar.gz` pattern that the workflow attests and uploads, sorts next to its sibling, and says what it is: CI's copy |
| `fil-<ver>-source.tar.gz` | the tagged tree with every dependency vendored (`cargo vendor`), which builds offline. It is the Corresponding Source for the binaries (LIC) |
| `fil_<ver>_amd64.deb`, `fil-<ver>-1.x86_64.rpm` | the x86_64 Linux archive's files as a Debian and an RPM package named `fil` (decided, owner 2026-11-04: D1; PKG, `docs/m4/packaging.md` §2). The rpm is signed with the package key (§10.4) |
| `fil_<ver>_arm64.deb`, `fil-<ver>-1.aarch64.rpm` | the same for aarch64 |
| `SHA256SUMS` | the checksums of the nine files above it: the four binary archives (the macOS one twice), the source archive and the four packages |
| `SHA256SUMS.sig` | an SSH signature over `SHA256SUMS`, made with `ssh-keygen -Y sign -n fil-release` by the release identity, the owner's own SSH key (M4 OQ-2; decided, owner 2026-11-04) |
| `SHA256SUMS.asc` | an OpenPGP signature over the same `SHA256SUMS`, by the package key (§10.4), for those who verify with gpg |

The GitHub build-provenance attestation for each archive (the `.ci` one included) and each package is attached to the release by the workflow. It is not a file in the list. The attestations of the two rpms are of the unsigned bytes CI built; after the owner signs them (§10.4), their provenance is the signature and the rebuild, which compares everything but the signature header.

### 4.2 `SHA256SUMS`

- **Format:** the output of GNU `sha256sum`: 64 lowercase hex digits, two spaces, then the file name with no directory part, one line per file, LF line endings.
- **Order:** sorted by file name, bytewise (`LC_ALL=C sort`).
- **Coverage:** exactly nine files: the four binary archives (the macOS one twice), the source archive and the four packages of §4.1. Not `SHA256SUMS` or `SHA256SUMS.sig`.
- **Check:** `sha256sum -c SHA256SUMS` (or `shasum -a 256 -c SHA256SUMS`) in the download directory. The signature is checked with `ssh-keygen -Y verify -n fil-release` against the allowed-signers file from `main`, never from the tag (REL's `release-verify`).

### 4.3 The archive layout

Every archive unpacks to a single top-level directory, named like the archive without `.tar.gz` (for example `fil-0.1.0-aarch64-linux-musl/`); REL's `release-check` asserts that an archive has exactly that one top-level entry. Relative to that directory, every archive has this layout:

```
bin/fil
libexec/fil/remote/x86_64-linux/fil
libexec/fil/remote/aarch64-linux/fil
libexec/fil/remote/SHA256SUMS
share/man/man1/fil.1
share/man/man5/fil-config.5
share/man/man5/fil-policy.5
share/bash-completion/completions/fil
share/zsh/site-functions/_fil
share/fish/vendor_completions.d/fil.fish
share/doc/fil/{README.md,CHANGELOG.md,LICENSE,THIRD-PARTY-NOTICES.md,config.example.toml,daemon.example.toml,policy.example.toml}
```

- **The generated files** come from `docs/man/build.sh`: `fil.1` and the three completions from the clap definitions (`crates/fil/examples/gen_docs.rs`, with `clap_mangen` and `clap_complete` as dev-dependencies only, so they are not in the release graph), and `fil-config.5` and `fil-policy.5` from `docs/user/config.md` and `policy.md` by pandoc. Pandoc is a build tool, not a dependency, and its version and SHA-256 are pinned in `docs/man/install-pandoc.sh` and `build.sh`: another version can write other bytes. The man pages carry no date, so they are reproducible. The committed `docs/man/fil.1` must equal the generator's output (`crates/fil/tests/docs_sync.rs`; `just docs-man` rewrites it). The three `*.example.toml` files are `docs/user/examples/{config,daemon,policy}.toml`.
- **`bin/fil`** is the target's binary: the static Linux binary, or the universal macOS binary.
- **The bundle** is `libexec/fil/remote/`. Every archive carries the static Linux binaries, so any client can install fil on either Linux architecture (U1). The bundle's directory is chosen from the remote's `uname -s` and `uname -m`, as the bootstrap's greeting reports them, by this table and nothing else:

  | `uname -s` | `uname -m` | Bundle directory |
  |---|---|---|
  | `Linux` | `x86_64` | `x86_64-linux` |
  | `Linux` | `aarch64` or `arm64` | `aarch64-linux` |
  | anything else, including `Darwin` | any | none |

  A `Darwin` remote never maps to a Linux target, whatever its `uname -m`. A 64-bit kernel with a 32-bit userland (32-bit Raspberry Pi OS on a Pi 4) reports `aarch64`, and the static aarch64 binary runs there, so that mapping is right.
  - **Byte-identical:** each copy is byte-identical to the binary of its target as it appears in that target's own archive (`bin/fil` of the Linux archive). REL's `release-check` checks that, in every archive.
  - **No macOS binary is bundled.** A Mac remote receives the client's own executable if that is the eligible universal release build, or the file named with `--binary` (U1, "Which binary is sent").
- **`libexec/fil/remote/SHA256SUMS`** is in the same format as §4.2, with paths relative to `libexec/fil/remote/`:

  ```
  <hash>  aarch64-linux/fil
  <hash>  x86_64-linux/fil
  ```

  Sorted bytewise. `sha256sum -c SHA256SUMS`, run in that directory, checks it. The client checks the file it is about to send against this list before sending.
- **How the client finds the bundle:** relative to its own canonical executable path (`bin/fil` is `../libexec/fil/remote` from `bin/`), so the layout survives Homebrew's Cellar and its symbolic links.
- **A client installed any other way** (`cargo install`) has no bundle. It can upload only itself, if it is eligible, or a file named with `--binary`.

### 4.4 Reproducible archives

- **The recipe, one for all archives, with GNU tar and gzip.** All four archives, the universal macOS one included, are assembled on the Linux `publish` job **inside the digest-pinned x86_64 Linux image that `Cross.toml` names** (the image REL pins for the `x86_64-unknown-linux-musl` target), not with the hosted runner's own tar and gzip, which the runner image can change. macOS's bsdtar has no `--sort`, which is the other reason. The macOS binary is built on a macOS runner and carried over as a file. REL's log records `tar --version` and `gzip --version` from inside the image, and `release-verify` rebuilds the archives in the same image.
  - `tar --sort=name --format=gnu --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner --mode='u=rwX,go=rX'` (executables and directories end up 0755 and other files 0644; a PAX format would also need `--pax-option=delete=atime,delete=ctime`, which is why the format is `gnu`);
  - `gzip -n -9`, so the archive carries neither a name nor a time and the compression level is fixed;
  - `SOURCE_DATE_EPOCH` is the tagged commit's committer time.
- **Pinned tools:** the tools that generate content are pinned by version and installed the way `linux-check` installs its tools (install-action with a version): `cargo-about` (LIC; it runs in `just notices`, never while packaging, §7) and pandoc (DOC).
- **The Linux binaries and archives must reproduce:** the two CI builds of a candidate give identical SHA-256s, and the owner's independent rebuild (`just release-verify`, in the VM) reproduces the binaries. macOS is best effort.
- **The .deb and .rpm packages** are made from the two Linux archives by nfpm 2.48.0 in its digest-pinned image (`tests/release/linux-packages.sh`), with every time set to `SOURCE_DATE_EPOCH`, and must reproduce like the archives: identical in the two assemblies, and in the rebuild (`docs/m4/packaging.md` §2).

## 5. The remote install layout

### 5.1 The directory

- **The path:** `$HOME/.local/share/fil/<ver>-w<major>.<rev>/fil`. For example, `~/.local/share/fil/0.1.0-w1.12/fil`. The version directory is mode 0700 and the file 0700.
- **The name carries the wire**, so garbage collection can tell which daemons still need a copy without running any binary.
- **`$HOME`-based, not `$XDG_DATA_HOME`.** A non-interactive login may see a different environment from an interactive one, and the path must not move between them. The state directory `~/.local/state/fil` follows the same rule.
- **A third place where fil writes on the remote.** The owner approved `~/.local/share/fil` in M4 OQ-1 (2026-11-04), and `DESIGN.md` §11.1.5 names it: fil creates nothing on the host beyond its state directory, its socket directory and, when the user installs fil through fil, its install directory. `DESIGN.md` §11, "Server binary distribution", describes the upload into this section's layout.
- **Never replaced in place.** A version is installed once, by a rename from a fresh directory. A new version gets a new directory. fil never modifies, replaces or removes a `fil` it did not install, such as one on `PATH` or one from a package manager.
- **Stamped when installed:** `--install-finish` sets the version directory's modification time when it puts it in place, because a rename does not change the moved directory's own time. GC reads that time (§6.3, "recently used").
- **Fresh directories:** an upload goes into `~/.local/share/fil/.new-<pid>` (the shell's `$$`), created with `mkdir -m 700` and renamed to its final name only after the size and SHA-256 are verified.
- **Leftovers.** A bootstrap killed mid-upload leaves its `.new-*` directory, which garbage collection removes once it is more than 24 hours old (§6), and any of `~/.local` and `~/.local/share` (mode 0700) that it had created. Those stay.

### 5.2 What the bootstrap and the client may assume

The bootstrap script (`docs/m4/plan.md` §3, U1; the exact text is M4 OQ-1) builds the directory name from its own constants: `v=<ver>-w<major>.<rev>`, with both parts checked at compile time against the pattern of §6.2 (the same check, so a build cannot install a directory that GC never recognizes). It `exec`s `fil server --stdio` from the exact-version directory only after the checks in the plan. Nothing in this file changes those checks.

## 6. FROZEN: the `--install-gc` contract

`fil server --install-gc` (the `remove` mode of the bootstrap, `fil install --remove HOST`) and the collection that `fil server --install-finish` runs after an install both select what to remove with **one** function, which implements this section. This section fixes what any copy of fil, now or later, may rely on when it judges another copy's directories. 0.1.0 is the only release whose rules every later copy has to honor, so a keep reason that 0.1.0 lacks can never be relied on later. That is why §6.3 has a "recently used" rule.

### 6.1 What GC looks at

GC looks at the entries of exactly one directory, `$HOME/.local/share/fil`, and at two files per wire in the socket directory. It lists nothing else.

- It never removes `~/.local/share/fil`'s parents (`~/.local/share`, `~/.local`): they belong to everything else on the host.
- `--install-gc` (and only `--install-gc`) removes `~/.local/share/fil` itself with `rmdir` when its pass has left it empty, because `DESIGN.md` §11.1.5 says fil removes what it no longer needs. `rmdir` fails on a non-empty directory, so it cannot remove anything that appeared in the meantime, and a bootstrap that races it fails cleanly at its `mkdir` of `.new-$$` (and removes only the directories its own run created). `--install-finish` never removes the root.

### 6.2 The pinned values

| Item | Value |
|---|---|
| Install root | `$HOME/.local/share/fil` (the environment's `$HOME`, which the bootstrap has already checked) |
| Version directory name | matches `^<ver>-w<major>\.<rev>$`, with `<ver>` = `[0-9][0-9A-Za-z.-]{0,63}` (a leading digit, 1 to 64 characters, no `+`), and `<major>` and `<rev>` each `[1-9][0-9]{0,4}` and at most 65535. The split is at the **last** `-w`, so a version may itself contain `-w` (`0.1.0-wip-w1.12` is version `0.1.0-wip`, wire 1.12) |
| Socket directory | `/tmp/fil-<uid>`, `<uid>` the effective user id in decimal (`DESIGN.md` §5.3) |
| Daemon lock file of a wire | `<socket directory>/fil-<major>-<rev>.lock` |
| Start lock file of a wire | `<socket directory>/fil-<major>-<rev>.start` |
| Fresh-upload directory | an entry named `.new-` followed by one or more characters |
| The `.new-*` age bound | **86400 seconds (24 hours)**: collected only when `now - mtime > 86400 s` of the directory itself. A modification time in the future is not old |
| The "recently used" window | **604800 seconds (7 days)**: a version directory is kept when `now - mtime < 604800 s`, or when its mtime is in the future. At exactly 604800 s it is not recent |
| The "far future" bound | **86400 seconds (1 day)**: an mtime more than this ahead of `now` is "far future". Plain GC still keeps it as recently used and reports it separately; a forced pass ignores it (§6.3, 2.5) |

`<major>` and `<rev>` in the lock names are the numbers from the version directory's name, in decimal, without leading zeros, as `fil_daemon::paths` writes them (`Paths::lock`, `Paths::start_lock`). This section states the same names for the readers of other versions.

**The compile-time check equals this pattern.** The remote command's compile-time check (§5.2) accepts exactly the version and wire that this pattern accepts. A build that would install a directory no GC recognizes does not compile.

**Why the bounds are what they are.** `now` is this host's clock, and an mtime is set by the file system, which on a shared (NFS) home is another host's clock. The 24 hours for `.new-*` and the 7 days for recency tolerate any realistic skew between hosts that share a home.

### 6.3 The selection

GC works through the entries of the install root and decides each one alone:

1. **An entry whose name matches the version-directory pattern** is a candidate only if it is a real directory (`lstat`: not a symbolic link) owned by the user. Anything else with such a name, including a directory owned by another user, is left alone.
2. **A candidate is kept if any of these holds,** and removed otherwise:
   1. **It is the current copy.** In `--install-finish`'s collection, that is the directory it has just installed or, when the same version was already installed and the new upload was discarded, the existing directory of that name. `--install-gc` exempts no directory by identity: it is `remove`, and it removes the copy it runs from too (the other rules still apply).
   2. **Its wire has a running daemon:** the file `fil-<major>-<rev>.lock` in the socket directory exists and is held (the probe is defined below). All directories of a wire are kept, whatever their `<ver>`, and whichever fil started the daemon: a daemon started from a binary that fil did not install keeps the directories of its wire too, which costs only disk.
   3. **Its wire's start lock is held:** the same probe on `fil-<major>-<rev>.start`, so GC never races a relay that is starting a daemon from that directory.
   4. **Unknown is keep:**
      - the probe could not decide (below);
      - the socket directory exists but cannot be examined, or fails the checks of `DESIGN.md` §5.3 (not a real directory, wrong owner, mode not exactly 0700).

      A **missing** socket directory or a missing lock file is "no daemon".
   5. **It was recently used:** its directory's mtime is less than 7 days old, or in the future. This is the only rule that covers what lock files on this host cannot show: a daemon on another host that shares the home, a copy that `fil install` has installed and nobody has attached to yet, and a concurrent install that has not reached its `exec`.
      - **Stamped at install:** `--install-finish` sets the version directory's mtime to the current time when it puts the directory in place (a rename does not change the moved directory's own mtime).
      - **Refreshed while in use:** a running daemon whose executable is a conforming version directory under the install root sets that directory's mtime to the current time on its hourly maintenance wake (TMP). It finds that directory from its own executable (`/proc/self/exe` on Linux, `current_exe()` on macOS).
      - **Two passes use this one function.** The **plain** pass is `--install-finish`'s collection after an install, and it applies every rule of this section. The **forced** pass is what `fil install --remove HOST` runs; it skips **rule 2.5 and nothing else**. The lock rules (2.2 and 2.3) and "unknown is keep" (2.4) still apply to it, so a running daemon's copy, a copy whose start lock is held and a copy the probe could not judge are never removed, forced or not. The current-copy exemption (2.1) is not in a forced pass, as before.
      - **The forced pass is the bare `--install-gc`.** The bootstrap's `remove` mode `exec`s exactly `fil server --install-gc`, with no arguments, and its text is fixed (plan, OQ-1). So `--install-gc` with no arguments is the forced pass, and the user's explicit `fil install --remove` is what makes it forced. Any other spelling (a flag that keeps recent copies, for example) is U1's to add and is outside this contract.
      - **A far-future stamp cannot pin a copy for good.** An mtime more than 1 day ahead of `now` (the clock was wrong at install) counts as recently used for the plain pass, which stays safe, and the plain pass's result line reports such copies separately. The forced pass ignores it like any other mtime, and a running daemon's hourly refresh sets the time back to the present.
      - Both passes report in their result line how many copies they kept as recently used.
3. **An entry named `.new-*`** is collected if it is a real directory (not a symbolic link) owned by the user, and its mtime is more than 24 hours in the past. Younger ones are never touched, so GC never removes another client's upload in flight. A process ID alone cannot tell uploads apart: IDs repeat across hosts that share an NFS home, and across containers.
4. **Every other entry** (files, symbolic links, directories with any other name, a `.new` without the dash) is never touched.
5. **Removal** of a selected directory is std's `fs::remove_dir_all` on its path, after the `lstat` check above. It has not followed symbolic links since Rust 1.58.1 (CVE-2022-21658), and the pinned toolchain is newer. A symbolic link inside a candidate is removed as a link and its target survives, and a test pins that. (An `openat`-based walk that never follows links would do as well.) A failure to remove is reported and does not stop the pass.

**The probe.** For a lock or start file path, the probe returns one of three answers, and it is not `fil_daemon::paths::lock_is_held` (which maps every failure to "not held", the opposite of rule 2.4; U1 writes its own probe, or gives that helper three states and maps "unknown" to "not held" for `fil ls`):

1. `open` it read-only with `O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC`. `ENOENT` is **absent** (no daemon). Any other error, a symbolic link's `ELOOP` included, is **unknown**.
2. `fstat` the open file. Anything but a regular file (a FIFO, a socket, a directory) is **unknown**, on every platform, before any `flock`.
3. Take a shared, non-blocking `flock`. `EWOULDBLOCK` is **held**. Success is **absent** (the lock is released at once). Any other error is **unknown**.

"Held" keeps the directory (rules 2.2 and 2.3). "Unknown" keeps it (rule 2.4). Only "absent" lets a directory fall through to the next rule. The probe creates, removes and connects to nothing, so it cannot disturb a daemon that is starting.

GC prints nothing that the user did not ask for beyond its result line; the line's format belongs to U1, not to this contract.

### 6.4 Changing a frozen item

A later version may **extend** collection only so that **an older copy's rules stay safe** for everything that the later version creates. That means:

1. **Names it recognizes: only a subset.** For any entry this section's rules recognize (a conforming version directory or a `.new-*` entry), a later version collects a subset of what §6.3 collects. It may add reasons to keep. It may not remove a directory that §6.3 keeps in a plain pass, and a later forced pass skips rule 2.5 only, never a lock rule or "unknown is keep". It may not shorten either time bound (a longer one is fine). A shorter `.new-*` bound would remove the upload in flight of an older client, which relies on the 24 hours; a shorter recency window would remove a copy that an older install stamped.
2. **Names it does not recognize: only its own kinds.** A later version may collect kinds of entries that this section does not recognize, but only entries of kinds it defines and creates itself, and never one whose name matches the version-directory pattern or `.new-*`. Older copies leave such names alone (§6.3, 4).
3. **Everything it installs follows the pattern and the clock.** A later version installs its binaries only as `<ver>-w<major>.<rev>` directories and stamps them at install (§6.3, 2.5). It stages uploads only in `.new-*` directories and finishes (renames) them within 24 hours. That bound implies a minimum upload rate: at the 128 MiB cap, about 1.5 KB/s; for a 15 MB binary, under 200 B/s. An upload that GC collects anyway fails cleanly: its `wc -c` check fails, the script removes what it created and exits 1. A test pins that.
4. **It keeps the signs of life.** For every wire that has an install directory:
   - a daemon of wire `<major>.<rev>` holds `fil-<major>-<rev>.lock` for its whole life, and a relay holds `fil-<major>-<rev>.start` while it starts one, in `/tmp/fil-<uid>`, under those names, in every later version, including after a change of `PROTOCOL_MAJOR`. Otherwise an older GC would find "no daemon" and remove a copy that a live daemon runs from. Keeping those files alive is TMP's job;
   - a running daemon refreshes its install directory's mtime at least once every 24 hours (the hourly wake of §6.3, 2.5, or any longer interval up to that).
5. **A change to the pattern, the file names, the socket directory or a time bound itself** is not an extension. It needs a new install root name (not `~/.local/share/fil`), so that older copies never see the new layout, and it goes to the owner, because it is a new place where fil writes (§5.1).

### 6.5 What the test must pin (U1)

U1 implements `--install-gc` and `--install-finish`'s collection, and writes the test that pins this section. The values are literals in the test, not derived from the code under test, so renaming a file or changing a bound fails it. They are also kept as a golden file, `crates/fil/tests/golden/install-gc.txt`: one `key = value` line per pinned value of §6.2 (the far-future bound included), and a `[history]` like the wire golden's (one line per revision of the contract, a hash of the pinned block, only ever growing), so an edit to a frozen value is visible in review and fails the test. §6.4's extensions may add lines only in a separate section of the golden, below the pinned block.

1. **The name pattern.**
   - Accepted, with their `(ver, major, rev)`: `0.1.0-w1.12` is (`0.1.0`, 1, 12); `0.1.0-rc.1-w1.12` is (`0.1.0-rc.1`, 1, 12); `0.1.0-wip-w1.12` is (`0.1.0-wip`, 1, 12); `1.0.0-w2-w1.12` is (`1.0.0-w2`, 1, 12); `1.2.3-w65535.65535`; a version of exactly 64 characters.
   - Refused (the entry is then left alone): `0.1.0`, `0.1.0-w1`, `0.1.0-w1.`, `0.1.0-w.12`, `0.1.0-w01.12`, `0.1.0-w1.012`, `0.1.0-w0.12`, `0.1.0-w1.0`, `0.1.0-w1.65536`, `0.1.0-w1.12.3`, `0.1.0+b-w1.1`, `-w1.12`, `.0.1.0-w1.12`, `0.1.0-w1.12/`, `0.1.0 -w1.12`, `0.1.0-W1.12`, a version of 65 characters, an empty name.
   - The remote command's compile-time check accepts and refuses exactly the same versions and wires (its const check is tested against this list).
2. **The lock names.** For `0.1.0-w1.12` and uid 1000: `/tmp/fil-1000/fil-1-12.lock` and `/tmp/fil-1000/fil-1-12.start`. A second assertion checks that `fil_daemon::paths::Paths` produces the same file names for wire 1.12, so the daemon and GC cannot drift apart.
3. **The `.new-*` bound.** A directory with mtime 86399 s old is kept, 86400 s is kept, 86401 s is collected, and one in the future is kept. A `.new-x` that is a symbolic link, a regular file or owned by another user is never touched. `.new`, `new-1` and `x.new-1` are never touched. An upload whose directory GC has removed fails cleanly (§6.4, 3).
4. **The recency rule, and the forced pass.**
   - A plain pass, candidate with no lock held and not current: mtime 604799 s old is kept; 604800 s and 604801 s are collected; one in the future is kept; one 86401 s in the future is kept and reported as far-future, and 86399 s ahead is kept and not so reported.
   - A forced pass (bare `--install-gc`): a candidate with mtime of the current time and no lock is **removed**; so are 604799 s old and one 86401 s in the future. With the daemon lock held, the start lock held, or an unknown probe, the same candidates are **kept**.
   - `--install-finish` leaves the new directory's mtime at the install time. Both passes report the number kept as recently used.
5. **Each other keep rule alone, and each removal** (with the mtime older than 7 days, so rule 2.5 does not hide the others):
   - a directory whose wire's lock is held (a real `flock` taken by the test) is kept, and another directory of the same wire with a different `<ver>` is kept too;
   - the same with only the start lock held;
   - the install-time current copy is kept with no lock held, and `--install-gc` removes the same directory;
   - a directory whose wire has a lock file that nobody holds, and one with no lock file, are removed;
   - a lock path that is a symbolic link, a FIFO or a directory is **kept** (unknown), on Linux and on macOS, and does not hang;
   - an unreadable or wrong-owner or wrong-mode socket directory keeps everything (unknown is keep);
   - a missing socket directory removes the unprotected candidates;
   - a version directory owned by another user, or that is a symbolic link, is left alone (use the pure selection function with injected metadata where the test cannot `chown`).
6. **Nothing else.** Files, symbolic links to directories, and directories with unknown names in the install root are untouched. A symbolic link inside a removed candidate is removed as a link, and the directory it points to outside the root, with its contents, survives. `~/.local/share` and `~/.local` are never removed. `--install-gc` removes `~/.local/share/fil` when the pass empties it, and `--install-finish` does not.
7. **Mutation table.** The PR's mutation table includes: change either time bound; drop rule 2.5 (the test of a copy with a fresh mtime and no lock on this host fails); let the forced pass skip a lock rule or "unknown is keep" (the forced-with-lock test fails); let the forced pass honor rule 2.5 (the forced fresh-copy test fails); shorten the lock name; treat a held start lock as "free"; follow a symbolic link at a `.new-*` entry or inside a candidate; treat an unreadable lock as "no daemon"; treat a non-regular lock as "no daemon"; split the name at the first `-w`. Each must fail the named test.

### 6.6 Why "recently used", and not the start lock

The lock files show only the daemons of this host. The plan's first design had two keep rules built on them, and they leave three cases unprotected: a copy whose daemon runs on **another host that shares the home** (host A's GC cannot see host B's `/tmp`), a copy that `fil install HOST` has installed with **no attach yet**, and a **concurrent install** by a client of another version, whose directory is renamed into place before its `exec`. Without a new rule, an install by one client removes another client's copy; on a macOS remote it could also remove the file that a running daemon starts its next session through (`current_exe()`; plan §1, the trampoline item).

Holding the **start lock** over such a copy does not work:

- the bootstrap is a shell script and cannot hold a lock across `exec`;
- `fil install` without an attach has no daemon to wait for, and nothing to hold the lock for;
- `--install-finish` holding the start lock through `exec … server --stdio` would make that relay's own `StartLock::acquire` wait on itself until `START_LOCK_TIMEOUT`.

The recency rule needs none of that: one install stamp, and one refresh an hour against a seven-day window, so it tolerates skew between hosts and costs at most one idle copy (about 15 MB) per version for a week.

A copy outside the window with no daemon on this host is collectable. If a daemon is still running from it, one of these holds: the daemon is not running (its host was off), **or** its host was asleep or suspended for a week (a suspended VM or laptop) so it has not refreshed, **or** its host's clock is more than 7 days behind the host that runs GC (a board with no RTC before NTP, for example). The consequence is the same in the last two cases:

- **Linux:** the daemon and its sessions go on (it runs from `/proc/self/exe`). The next connection to that host finds no copy, and the client re-uploads the same wire (one upload and, with `install = "ask"`, one prompt). The relay then reaches the live daemon.
- **macOS remote:** existing sessions survive, but new sessions started in that daemon fail until the copy is installed again, because the daemon starts them through `current_exe()`, a path that no longer exists (plan §1, the trampoline item).

That cost is **accepted** for 0.1.0, given its preconditions: a home shared across hosts, mixed versions, and a week of sleep or a clock that far behind.

## 7. Notices (LIC)

- **The file.** `THIRD-PARTY-NOTICES.md` is committed at the root of the tree. Every binary archive carries it as `share/doc/fil/THIRD-PARTY-NOTICES.md`, beside `LICENSE` (§4.3), and the source archive carries it at its root.
- **What it covers.**
  - **The release graph:** the `fil-shell` package (`crates/fil`) with its default features, its normal and build dependencies, on the four release targets of §3 (`targets` in `about.toml`). That is the graph `release-deps` checks (`cargo tree -p fil-shell -e normal,build`), narrowed from `--target all` to the targets fil ships: a crate that only Windows, wasm or UEFI links is in no release binary. For each crate: its name, version and the license expression its manifest declares. Then the license texts, taken from the crates' own files, so the copyright lines are theirs, each with the crates it covers. Where a crate offers a choice (`MIT OR Apache-2.0`), fil takes the first license of `about.toml`'s list that it offers, and the file carries that text.
  - **Clarifications:** a crate whose license file cargo-about does not recognize gets a `[CRATE.clarify]` in `about.toml`, pinned by the checksum of the text it takes. Today that is rcgen, whose one `LICENSE` file holds both its MIT and its Apache-2.0 text. Without the clarify, cargo-about would print the canonical MIT text with `<year> <copyright holders>` in place of rcgen's own copyright line. `deny.toml` needs none: cargo-deny reads rcgen's license from its manifest.
  - **Copyright notices in source files:** the crates whose license includes ISC (ring, rustls-webpki, untrusted, and rustls, which offers it) carry copyright lines in their files' headers from more authors than their license files name: Google, Intel, David Judd, Trent Clarke, Simon Sapin, Rafael Fernández López, Daniel McCarney and others. `tests/release/notice_scan.py` collects every `Copyright` line of every file with the ISC permission notice in each crate whose license includes ISC, and the recipe writes them into their own section. A new holder therefore makes the committed file stale. Files under Apache-2.0 headers (ring's OpenSSL and BoringSSL code) need nothing more in object form than the license text and a NOTICE file, and none of the crates has one (below).
  - **More than one binary holds:** build scripts and procedural macros, which run while fil is built, are listed too, and a crate that one target alone uses is only in that target's binary. The workspace's own crates are not listed: they are fil, under `LICENSE`.
  - **Two notices that are not crates,** written by hand in `about.hbs`:
    - **the Rust standard library** (MIT OR Apache-2.0; fil takes it under MIT, with the copyright line of Rust's `LICENSE-MIT`). The toolchain version is filled in from `rust-toolchain.toml`. For the crates the standard library is built from and the Unicode data in `core`, the file points at the Rust release's `COPYRIGHT-library.html`, which lists each with its notice. The LLVM runtime code the toolchain links in (parts of compiler-rt in `compiler_builtins`, and in the Linux binaries LLVM's libunwind, `crtbegin.o` and `crtend.o`) is Apache-2.0 WITH LLVM-exception, whose exception needs no notice for code embedded in object form; the file says so and points at `COPYRIGHT.html`, where Rust lists it under `src/llvm-project`;
    - **musl** (MIT), statically linked into the Linux binaries from the Rust toolchain's musl targets (musl 1.2.5): its `COPYRIGHT` file, verbatim.
- **How it is made.** cargo-about (MIT OR Apache-2.0; a build tool, in no binary), **pinned to 0.9.3** in the justfile (`about_version`) and installed the way `linux-check` installs its tools (install-action with a version; on Intel macOS, which it ships no binary for, `cargo install --locked` of the same version). Its config is `about.toml` and its template `about.hbs`. `just notices-update` runs `cargo about generate --frozen --fail -m crates/fil/Cargo.toml`, drops carriage returns, fills in the toolchain and the source-file notices (`tests/release/notice_scan.py`, over `cargo metadata`), and writes the file. Review the diff and commit it with the change that caused it.
- **Deterministic.** `--frozen` runs cargo-about offline: no license text is fetched from a git host. The output is in cargo-about's sorted order, and the source-file notices are sorted too. So the bytes depend only on `Cargo.lock` (and the crates it pins by checksum), `about.toml`, `about.hbs`, `notice_scan.py`, `rust-toolchain.toml` and the cargo-about version, never on the machine; the recipe refuses a render that contains a local path. Because the file is committed, packaging never runs cargo-about: `package.sh` copies it from the tree like `LICENSE`, and the archives stay reproducible (§4.4).
- **`just notices`** (part of `just ci`, right after `deny`, and a step of `release.yml`'s `prepare` job; it needs cargo-about and python3) fails when:
  - cargo-about is not the pinned version;
  - `about.toml`'s `accepted` list is not `deny.toml`'s `[licenses] allow` list (compared as sets; the order in `about.toml` is the priority above);
  - a crate in the release graph has a license outside that list, or one cargo-about cannot read (`--fail`);
  - cargo-about prints a `WARN` or `ERROR` line. A clarify whose checksum no longer matches (after a bump of that crate) only warns, and cargo-about then falls back to the canonical text;
  - a rendered text still has a template field (`<year>`, `<copyright holders>`): a license file cargo-about did not recognize, which needs a clarify;
  - a crate in the graph ships a `NOTICE` file whose text is not in the notices (Apache-2.0 §4(d));
  - the crates the notices list are not exactly the registry crates of `cargo tree`'s graph for the same targets;
  - the committed file is not byte for byte the render: stale, or edited by hand.

  `release-check` requires the file in every binary archive and in the source archive, and each binary archive's copy to be the source archive's.
- **When to regenerate:** after any change to `Cargo.lock` that reaches the release graph, a toolchain bump (`rust-toolchain.toml`) or a cargo-about bump (change `about_version` and the workflows' pins together). A new license is a policy change: it goes into `deny.toml` and `about.toml` together, and only if it is GPL-3.0-compatible (`CLAUDE.md`).
- **Corresponding Source** (GPL-3.0 §6). Every release publishes `fil-<ver>-source.tar.gz` beside the binaries, in the same place and at the same time (§4.1): the tagged tree, with the scripts that build and package it (`tests/release/`, `Cross.toml`, `rust-toolchain.toml`), and every dependency vendored (`cargo vendor`, with the `.cargo/config.toml` that builds offline). It is the Corresponding Source of all three binary archives, and it also carries the source of every dependency, so the source terms of the MPL-2.0 and LGPL crates, if any are ever added, are met by the same file. `release-check` checks that it resolves every dependency offline, and the release notes point at it (`tests/release/notes.sh`). It stays published as long as the release does.
- **`--version`.** `fil --version` and `fil -V` print the same text: the first line, `fil X.Y.Z (wire A.B)` (scripts, the bootstrap and `release-check` read it with `head -n 1`; it never changes), then GPLv3's "Appropriate Legal Notices" in the GNU style: `Copyright (C) 2026 Anthony Wertz` (the holder of OQ-2), `License GPL-3.0-or-later <https://gnu.org/licenses/gpl.html>` and the no-warranty sentence. The text is in `crates/fil/src/cli.rs` (`full_version_string`). `release-check` greps each binary for the three lines, because `--no-exec` runs nothing it downloaded.
- **No SPDX header in every source file** (proposed): the root `LICENSE`, every crate's `license` field and the notices are enough. REUSE compliance can come later.

## 8. Who builds against this file

| Package | Uses |
|---|---|
| REL | §1 (tag, version, wire checks), §3, §4 |
| U1 | §2, §4.3 (the bundle), §5, §6 |
| PKG | §3, §4 |
| LIC | §2 (the first line stays), §4.3 (`THIRD-PARTY-NOTICES.md`), §7 |
| DOC | §4.3 (the layout of the docs and completions), §5, §6 (for the user guide's removal instructions) |
| TMP | §6.3 (2.5) and §6.4, item 4 (the lock files that GC reads, and the install directory's refresh on the hourly wake) |

## 9. Building, verifying and signing a release (REL)

The scripts are in `tests/release/`; the workflow is `.github/workflows/release.yml`; the recipes are `just release-build`, `release-check`, `release-verify` and `release-test`.

### 9.1 What the workflow does

- **A tag push `v*`** builds the files of §4.1 and opens a **draft** GitHub Release with the nine files (the four archives, the four packages and `SHA256SUMS`) and a build-provenance attestation for each archive and package. It never publishes. **A dry run** (`gh workflow run release.yml --ref BRANCH`) does the same builds and checks, uploads the files to the run (artifact `release-files`) and does nothing else: its jobs have no `id-token` or `attestations` permission.
- **Jobs:** `prepare` (version, `SOURCE_DATE_EPOCH`, the tag checks, `release-deps`, `notices`, `release-test`, `crates-check`), `build-linux` (each Linux target twice, in two jobs), `build-macos` (twice, best effort), `package` (identical-binaries gate, archives and packages assembled twice in the pinned images, identical-files gate, `release-check`, the amd64 packages' install test), `smoke-macos` (runs the macOS archive and `release-check` on macOS), `packages-arm64` (the arm64 packages' install test, on an arm64 runner), `publish` (tag push only: the only job with `contents: write`, `id-token: write` and `attestations: write`) and `crates-io` (tag push only, after `publish`: the nine crates by trusted publishing, in the `crates-io` environment that the owner approves, with `id-token: write` and nothing else; §10.1, `docs/m4/packaging.md` §3).
- **The tag checks in `prepare` are not the gate.** On a tag push Actions runs the workflow file of the tagged commit, so whoever can push a `v*` tag controls it. The gates are the tag ruleset (OQ-2), the draft that a person publishes and the owner's `release-verify` and signature.
- **A release needs the complete layout.** `release-check` requires every file of §4.3 in every archive, in a dry run too (`--complete` is kept for the callers that pass it). DOC's files are produced into a directory laid out like an archive by `docs/man/build.sh` (`just docs OUT`), which the package job runs and passes to `package.sh --extra`; the rebuild VM makes the same tree with the same pinned pandoc (`rebuild.sh`). LIC's `THIRD-PARTY-NOTICES.md` is committed and comes from the tree, like `LICENSE` (§7), and is required in every archive already.
- **Version and tag:** the archive names carry the version in `Cargo.toml`; a dry run of a branch is named after it (`0.0.0` before the first release). `release-check` (non-dry) requires the tag to be `v<version>`, a `CHANGELOG.md` section for it and, for a patch release, the wire of the previous tag of the same minor.
- **Paths.** `trim-paths` is not stable in the pinned cargo, so the builds pass `--remap-path-prefix` flags (`tests/release/common.sh`): the workspace becomes `/build`, `CARGO_HOME` `/cargo-home` and the rustup home `/rustup-home`. `release-check` requires `/cargo-home/registry/src/` to appear in each binary, as proof that the remap was applied. When the pinned cargo stabilizes `trim-paths`, set it in `[profile.release]` and drop the flags.

### 9.2 The owner's steps for one release

**Repository settings** (the `v*` tag ruleset that only the owner passes, immutable releases, the code-owner review below, and the others of `docs/m4/plan.md` §5): the owner sets them before the first tag (owner, 2026-11-04).

**Once, before the first release: the crates.io names are reserved** (published by the coordinator; `just reserve-crate-names` is the script, §10.1), the `crates-io` environment exists and each of the nine crates has a trusted publisher (N8, §10.1), and, for v1, sign and notarize the macOS binary (`just release-sign-macos`, §10.3; it needs the Developer ID certificate and the `fil-notary` notarytool profile on the owner's Mac, and `.github/macos-team-id` on main).

**The release key.** It is a **dedicated** ed25519 key on the owner's account, used for nothing else, never the agent seat's `~/.ssh/id_ed25519` (which signs agent commits globally). Give it a passphrase (or use an `ed25519-sk` hardware key) and keep a backup. The signing commands below name it explicitly, so a global signing key is never used by accident. The key never leaves the owner's machine, and CI has no signing key and no secret.

0. **Once: the key goes into the repository.** Before the first release, a PR adds the key's public half to `.github/release-signers` on `main`, two lines (`namespaces="git"` for the tag and `namespaces="fil-release"` for the checksums; the file shows the form), reviewed by a code owner (`.github/CODEOWNERS`, with the "require review from Code Owners" ruleset setting). Until then the file is a placeholder with no key: `release-verify` stops at the tag, and the workflow warns. Publish the key's fingerprint out of band (README, site) so that verifiers can check `.github/release-signers` against it; a verifier may keep its own copy and name it in `FIL_RELEASE_SIGNERS`.
1. **Candidate.** Run the dry run from `main` (`gh workflow run release.yml --ref main`), then rebuild it in the VM: `just release-verify VERSION --dry-run --from-run RUN --ref SHA` (the full 40-hex commit SHA, which `release-verify` and the rebuild both insist on; §9.3). Nothing from the download is run on the owner's machine in this mode either.
2. **Tag.** On a clean `main` at the release commit, with the version and `CHANGELOG.md` already merged, make an annotated tag signed with the release key and push that one tag:

   ```sh
   git -c gpg.format=ssh -c user.signingkey=~/.ssh/fil-release.pub tag -s vX.Y.Z -m "fil X.Y.Z"
   git push origin vX.Y.Z
   ```

   The workflow builds the draft. If the `publish` job has to be re-run, delete the stale draft first, or there will be two.
3. **Verify, before signing.** `just release-verify X.Y.Z --pre-sign`. (It can be run again at any later point, after macOS signing too.) It downloads the draft and **stops at the first failure of its first three steps**, before looking at, let alone running, anything in it: the tag against `.github/release-signers` **on main** (annotated, signed, on main; its commit is resolved once and every later step is bound to that commit), `SHA256SUMS` against the files, and the attestations (`gh attestation verify` pinned to the repository, the workflow identity `…/release.yml@refs/tags/vX.Y.Z`, the commit as source and signer digest, and no self-hosted runner). `--pre-sign` skips only the signature, which does not exist yet. Then it runs `release-check --tag vX.Y.Z --complete --no-exec` (the files are unpacked, hashed, grepped and inspected; nothing from the download is run: no `cargo` in the source archive, no `--version` of a binary) and rebuilds the Linux binaries, the four archives and the four packages from that commit in the VM and compares hashes with the draft's. The macOS archive is only *reassembled* around the released macOS binary, which is not rebuilt.
4a. **Sign and notarize the macOS binary** (§10.3): `just release-sign-macos X.Y.Z`. It works in the copies step 3 verified, replaces the macOS archive and `SHA256SUMS` there, signs `SHA256SUMS` and uploads the three files to the draft.
4. **Sign the verified files.** Sign the copies that step 3 verified (`target/release-verify/X.Y.Z/files/`), not a fresh download, because any writer can replace draft assets in between. First the two rpms (§10.4), which rewrites `SHA256SUMS` and writes `SHA256SUMS.asc` (after macOS signing, §10.3, for v1: both rewrite `SHA256SUMS` and `SHA256SUMS.sig`, packages last, so sign `SHA256SUMS` again here); then `SHA256SUMS` with the release key; then upload what changed:

   ```sh
   just release-sign-packages X.Y.Z                # the rpms, SHA256SUMS, SHA256SUMS.asc (gpg asks for the passphrase)
   ssh-keygen -Y sign -f ~/.ssh/fil-release -n fil-release target/release-verify/X.Y.Z/files/SHA256SUMS
   cd target/release-verify/X.Y.Z/files
   gh release upload vX.Y.Z --clobber fil-X.Y.Z-1.x86_64.rpm fil-X.Y.Z-1.aarch64.rpm SHA256SUMS SHA256SUMS.asc SHA256SUMS.sig
   cd -
   just release-verify X.Y.Z                       # now the signature is checked too
   gh release edit vX.Y.Z --draft=false            # publish
   just release-verify X.Y.Z --no-rebuild          # once more, on what is published
   ```

5. **crates.io.** Approve the waiting `crates-io` deployment of the tag's run (Actions, the run, "Review deployments"). The job refuses to run unless the `crates-io` environment has a required reviewer and deploys from `v*` tags only, the tag still names the run's commit, and the release is published with a `SHA256SUMS.sig` that verifies against `.github/release-signers` on main; then it publishes the nine crates in dependency order without compiling anything (`docs/m4/packaging.md` §3).
6. **Homebrew and the rest.** `just brew-formula X.Y.Z` and `just aur-pkgbuild X.Y.Z` render the formula and the PKGBUILD from the `SHA256SUMS` that step 3 verified; the formula goes to the tap as a PR (`docs/m4/packaging.md` §4).

### 9.3 The rebuild VM

`release-verify` rebuilds in the Lima VM (`docs/dev/vm.md`; `FIL_VM` or `--vm` names it). The `cross` images are linux/amd64 and the VM is aarch64, so the amd64 containers run **under emulation that can run `rustc`: enable Rosetta in the VM.** The stock `qemu-user-static` of Ubuntu 24.04 (8.2) does not: `rustc` dies at startup with `QEMU internal SIGSEGV` while opening `/proc/self/maps`, so every `cross` build fails (found while building REL; hence the requirement). In the VM's `lima.yaml`:

```yaml
vmType: vz
vmOpts:
  vz:
    rosetta:
      enabled: true
      binfmt: true
```

Apply it with `limactl stop fil`, `limactl edit fil` and `limactl start fil`, or create a second instance for the rebuilds and name it with `--vm`. The rebuild installs, in the VM, `cross` 0.2.5 and `just` (`cargo install --locked`) and the pinned toolchain for `x86_64-unknown-linux-gnu` (`rustup toolchain install --force-non-host`; `cross` mounts it into the amd64 image) the first time, and clones the repository over HTTPS: the VM needs the network and nothing from the Mac. The macOS binary is the one input that is not rebuilt: the macOS archive is reassembled around the release's own `bin/fil`.

## 10. Names, channels and macOS notarization

### 10.1 Reserving the crates.io names

**Decided (owner 2026-11-04, round 2):** each published crate gets a 0.0.1 placeholder now, to reserve its name. (Round 2 also said "no crates.io token goes to CI"; N8 below replaces that, as the coordinator's choice on the owner's delegation: trusted publishing puts a short-lived OIDC token in CI, and there is no long-lived token anywhere.) The test-only crates (`fil-testkit`, `fil-testbins`, `fil-netsim`) stay unpublished. On the AUR the package is `fil-shell-bin`, because `fil` is taken there (decided, owner 2026-11-04).

**The rename (built by PKG).** `crates/fil` is the package `fil-shell`; its binary and its library are still `fil`, and the directory is unchanged, so `cargo build -p fil-shell` builds `target/*/fil`. The test-only crates are `publish = false`; the nine published ones are `publish = ["crates-io"]` and depend on each other with exact versions (`docs/m4/packaging.md` §3).

**The names.** The owner's list began with `fil` and eight `fil-*` libraries (`fil-proto`, `fil-transport`, `fil-session`, `fil-daemon`, `fil-client`, `fil-term`, `fil-predict`, `fil-forward`). **`fil` is unavailable on crates.io:** an unrelated crate (a random-number helper) took it on 2026-11-04 at 08:24 ET. The user-facing crate is therefore **`fil-shell`** (`cargo install fil-shell` installs the `fil` binary, matching `fil-shell-bin` on the AUR), reserved by the coordinator. **The name is not marked decided:** the owner can pick another. The workspace's `crates/fil` package must be renamed to match before it is published; the binary stays `fil`. Until then no document may suggest `cargo install fil`, which installs the wrong crate.

**Who publishes.** The owner first meant to publish the placeholders from their own account on their Mac. Later on 2026-11-04 the owner changed that: the coordinator published the placeholders from the `splarg-bot` crates.io account and invited `anthonytw` as an owner of each crate. That account's token has since been revoked and removed.

**How releases are published (N8; owner delegated the choice, 2026-11-04; coordinator chose trusted publishing).** crates.io releases use trusted publishing. A publish job in the release workflow uses `rust-lang/crates-io-auth-action`, which exchanges the job's GitHub OIDC identity for a short-lived crates.io token that is revoked when the job ends. The job runs behind a `crates-io` GitHub environment that only the owner can approve, from the signed `v*` tag. **No long-lived crates.io token exists anywhere**, neither on the owner's Mac nor in CI. This replaces N2's "v1 is published the same way, from the owner's Mac"; N2's other parts stand (the nine crates, the unpublished test-only crates, `fil-shell`). **Owner steps before the first tag, in this order:** (1) create the `crates-io` environment and protect it (required reviewer: the owner; deployment branches and tags: `v*` tags only). Do this first: GitHub creates a missing environment that a workflow names, with no protection, and a trusted publisher pins only the workflow file and the environment, not the ref, so an unprotected `crates-io` environment would let any writer's branch mint a token. (2) Then add a trusted publisher (this repository, the workflow `release.yml`, the environment `crates-io`) on each of the nine crates: `fil-shell` and the eight `fil-*`. The job also checks (1) itself, with `actions: read`, and fails unless the environment's required reviewers include the owner (`anthonytw`, a user reviewer) and its deployment refs are exactly the tag pattern `v*` (`deployment-branch-policies`); `prepare` makes the same check on every run (a warning on a dry run, an error on a tag push), so a dry run proves that the job's token can read the environment. The job is release.yml's `crates-io` (built by PKG, `docs/m4/packaging.md` §3).

**The script** (`tests/release/reserve-crate-names.sh`, `just reserve-crate-names`) does the same job and is resumable, for the case that a run is interrupted or a name is added:

1. After `cargo login`, `just reserve-crate-names` is a dry run, the default: it checks which names are free (a GET on `https://crates.io/api/v1/crates/<name>`, with a descriptive User-Agent) and runs `cargo publish --dry-run` for each placeholder. Nothing is uploaded.
2. `just reserve-crate-names --publish` asks for a "yes", then publishes. A name that is already registered is skipped with a message, not a failure.
3. `--account LOGIN` names the publishing account. With it, a taken name whose owners do not include LOGIN is reported as "taken by others", with its owners, in a list of its own, and the run exits 1.
4. `--invite OWNER` runs `cargo owner --add OWNER CRATE` after each publish, and for the crates already published, unless OWNER already owns the crate or was invited. A crate that the publishing account does not own is never invited to.

Each placeholder is built in a fresh temporary directory outside the repository (a `Cargo.toml` with the version 0.0.1, `GPL-3.0-or-later` and the repository URL; a `src/lib.rs` that is one doc comment; a `README.md`; no author, no email). The real workspace crates are never published by this script.

**Pacing.** crates.io limits new crates per account to a burst of 5, then one more every 10 minutes (crates.io source, `src/rate_limiter.rs`, `LimitedAction::PublishNew`: `default_burst` 5, `default_rate_seconds` 10 * 60; the answer is HTTP 429, "You have published too many new crates in a short period of time"). Nine names take about 40 minutes: the script publishes five at once, then waits 10 minutes before each of the rest, and on a 429 it waits and retries instead of failing.

### 10.2 The v1 channels

**Decided (owner 2026-11-04, round 2), superseding the narrower recommendation of `docs/m4/plan.md` OQ-3** (the owner's note: "Reserve names and prep packages everywhere for v1"). The channels, as the owner chose them:

- GitHub Releases and the Homebrew tap `anthonytw/homebrew-tap` (as recommended);
- crates.io, nine crates: `fil-shell` and eight libraries (§10.1), published by trusted publishing (N8);
- `.deb` and `.rpm` for x86_64 and arm64, built by CI and attached to the release;
- the AUR package `fil-shell-bin`;
- a Nix flake in the repository, plus a nixpkgs PR;
- how each channel is built, checked and updated: `docs/m4/packaging.md` (PKG);
- the start of the official distro submissions: Debian (an ITP, then an RFS; Ubuntu inherits Debian's package) and Fedora (a package review). These need owner accounts and a sponsor and finish whenever the distros finish. **v1 does not wait on their acceptance** (coordinator's reading, not the owner's words).

### 10.3 macOS: signed and notarized on the owner's Mac

**Decided (owner 2026-11-04, round 2; N4):** v1 is signed and notarized on the owner's Mac, and Apple keys are never in CI. `just release-sign-macos` (`tests/release/sign-macos.sh`) is built and tested on fakes in `just release-test`; its parts were run for real on an Apple-silicon Mac with an ad hoc identity (below).

**What CI leaves.** CI builds the macOS binary reproducibly (§9) and signs it **ad hoc** with the fixed identifier `fil` (an arm64 binary without a signature is killed), into a draft release, as `fil-<ver>-universal-apple-darwin.tar.gz` and, byte for byte the same, `fil-<ver>-universal-apple-darwin.ci.tar.gz` (§4.1). No certificate, no secret. The attestation covers both.

**The owner's steps**, after `just release-verify X.Y.Z --pre-sign` has passed in full (§9.2, step 3) and before the packages are signed (§10.4):

```sh
just release-sign-macos X.Y.Z      # Developer ID signature, notarization, new macOS archive and SHA256SUMS, signed, uploaded to the DRAFT
```

It needs, on the owner's Mac: the "Developer ID Application: Anthony Wertz (6X3PT3FXGA)" certificate and key in the login keychain (`FIL_CODESIGN_IDENTITY` names another certificate, by name, not by hash), a `notarytool` keychain profile named `fil-notary` (`xcrun notarytool store-credentials fil-notary`, once), the release key `~/.ssh/fil-release` (`FIL_RELEASE_KEY`; it asks for the passphrase), `gh`, and Docker (the archive is repacked by the same pinned GNU tar and gzip that CI uses, `pack_archives` in `common.sh`). It works in `target/release-verify/X.Y.Z/` and downloads nothing. In order:

1. **What was verified.** `target/release-verify/X.Y.Z/verified` must say the work directory was verified in full (the VM rebuild included, `--pre-sign` or not) and `SHA256SUMS` there must be the one that was verified. The local checks are run again on those files (`release-verify --from-dir`: the tag, the sums, the attestations of every archive including the `.ci` one, `release-check`). Nothing is signed if any fails.
2. **Draft only, and the draft is what was verified.** A release that is not a draft is refused. The draft's assets (digests from GitHub, or downloaded and hashed) must be the verified files, so that the release key never signs a `SHA256SUMS` over files that no rebuild judged. The one allowed difference is the result of an earlier run of this recipe that stopped part way.
3. **Extract and check the packing.** The macOS binary comes out of the `.ci` archive. The `.ci` archive is repacked with its own, unchanged binary and must come back byte for byte: this machine packs as CI does, found out before the notarization, which cannot be taken back.
4. **Sign.** `codesign --force --options runtime --timestamp --identifier fil --sign IDENTITY` (`--timestamp=none` for the identity `-`, an ad hoc run to try the script). It then reads the signature itself (`macho_norm.py info`) and requires, in every slice: the identifier `fil`, exactly the hardened runtime flag, the Team ID in `.github/macos-team-id` (6X3PT3FXGA), no entitlements, and, from `codesign -dvv`, the certificate's name and a secure timestamp. It also requires `codesign --verify --strict -R="anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] and certificate leaf[field.1.2.840.113635.100.6.1.13] and certificate leaf[subject.OU] = \"TEAM\""` (`dev_id_requirement` in `common.sh`): a Developer ID CA, a Developer ID Application leaf, and the team in the certificate itself, not only the Team ID written in the CodeDirectory. Checked on a Developer-ID-signed app: the right team passes, another team and an ad hoc binary fail.
5. **Only the signature changed.** The signed binary and the CI binary must normalize to the same bytes (see "Verifiability").
6. **Notarize.** The binary goes in a zip made by `ditto` (checked to hold the signed binary) to `xcrun notarytool submit --wait --keychain-profile fil-notary --output-format json`. The verdict must read `"status": "Accepted"`: `notarytool` can exit 0 for an `Invalid` submission, so the exit status is not trusted. The submission id, the verdict and Apple's log are kept in `target/release-verify/X.Y.Z/macsign-logs/` and never uploaded. Until Apple accepts, the downloaded files are untouched.
7. **Rebuild.** The `.ci` archive is repacked around the signed binary, `SHA256SUMS` is rewritten (`sums_check` must pass) and signed with the release key (`ssh-keygen -Y sign -n fil-release`). The results are staged in `macsign-staged/`.
8. **Check the signature** against the signers (`.github/release-signers` on main, as `release-verify` fetched it).
9. **Upload** the archive, `SHA256SUMS` and `SHA256SUMS.sig` to the draft with `gh release upload --clobber`, and only then copy them over the verified files, so that `just release-sign-packages X.Y.Z` (§10.4) continues from them. `SHA256SUMS.sig` then covers unsigned rpms until §10.4 is done and `SHA256SUMS` is signed again (§9.2, step 4); that is harmless while the release is a draft.

**If the upload stops half way,** the draft holds the signed archive and the old `SHA256SUMS`. Run the recipe again: the draft still has CI's `.ci` archive, which is all it needs, and the verified files were not touched, so the check in step 2 accepts the draft as it is. It signs and notarizes once more. `release-verify --pre-sign` also works on a draft in that state, and after the recipe has finished.

**Verifiability.** A signature cannot be compared with `codesign`. Removing it (`codesign --remove-signature`) leaves a header field, the `__LINKEDIT` segment's `vmsize`, at the value the signer chose from the size of the signature it wrote, the fat slice sizes follow, and `codesign` versions choose different page sizes and reserve different space: the CI runner's `codesign` (macOS 15) signs the same code with 4 KiB pages and a 82000-byte signature where this Mac's (macOS 27) uses 16 KiB pages, so neither CI's signature nor a rebuilt one can be reproduced on another Mac. So the comparison is done on the bytes, by `tests/release/macho_norm.py` (`python3 -I`, standard library only, any OS, Linux included). For each slice of a thin or fat 64-bit Mach-O it cuts at `LC_CODE_SIGNATURE`'s `dataoff`, removes that load command (later commands move up, the freed bytes are zeroed), sets `__LINKEDIT`'s file size to end where the signature began and its VM size to that rounded up to the architecture's page, and lays a fat file out again with the architecture's alignment. It refuses (exit status 2) anything it cannot parse or does not expect: an unknown magic, a signature that is not the last thing in its slice, a segment or a load command (symbol table, dyld info, linkedit data) that reaches into the signature, a `__LINKEDIT` `vmsize` that is not its signed file size rounded up to a page, a fat entry whose `cputype` is not its slice's, bytes after the last slice, non-zero padding. The archive comparison also compares the tar headers (the format, pax extended headers such as xattrs) and refuses data after the end of the archive; the gzip header is not compared, and `SHA256SUMS.sig` binds the bytes. The same code gives the same bytes whether it came from the linker, from CI's ad hoc `codesign` or from a Developer ID signature, whatever the page size.

`release-verify` uses it three ways, none of which needs a Mac:

- **The archive.** The macOS archive must equal the `.ci` archive in every member and every piece of tar metadata but `bin/fil`, and the two `bin/fil` must normalize to the same bytes (`verify-macos.sh agree`, also run by `release-check`). The `.ci` archive's attestation is checked directly, with no re-signing anywhere; the signed archive's is not (CI never attested it), and it is tied to the `.ci` one by this comparison instead.
- **The signature.** `verify-macos.sh report` reads the signature itself on any OS (one line per slice: identifier, flags, Team ID, entitlements) and requires, for a signed release, exactly the hardened runtime flag, the Team ID from `.github/macos-team-id` **on main** (`FIL_MACOS_TEAM_ID` overrides it, with a NOTE), no entitlements and a certificate chain. Where `codesign` exists it also requires `codesign --verify --strict`, the same designated requirement as `release-sign-macos` (the certificate's OU is the Team ID), a Developer ID Application authority and a secure timestamp. On a machine without it, it lists what it could not check (the certificate chain, the signing time) and that nobody checks notarization except Apple: Gatekeeper asks Apple online the first time the binary runs.
- **Your own build.** `--mac-rebuild FILE` (a universal macOS `fil` built from the tag) is compared with the released binary through the normalizer. It says whether they are byte-identical and, if not, how many bytes differ and which slice.

**Why the normalizer, and not codesign, and whether CI's own signing needs to match.** An earlier version of this section had the verifier re-sign the released binary ad hoc and compare with CI's archive, and CI sign through the same canonical form. Reviewing it, the coordinator found that the dry run's (macos-15, Xcode 16.4) binary re-signed on this Mac does not reproduce, even with `--pagesize` pinned, because the signature reserves different space. CI therefore signs as it always did (`codesign --force --sign - --identifier fil`, deterministic on one runner, which is all the identical-files gate in `release.yml` needs), and nothing depends on any other `codesign` matching it.

**Evidence.** On this Mac (macOS 27, aarch64 and x86_64 release builds joined with `lipo`) and on the binary of dry run 41899619926 (macos-15): the linker-signed build, an ad hoc re-sign (16 KiB pages), an ad hoc re-sign with `--pagesize 4096` and the hardened runtime, and the CI binary re-signed all normalize to the same bytes as their own sibling forms (a fat file with an unsigned x86_64 slice and a linker-signed arm64 one, and CI's with ad hoc 4 KiB-page signatures on both); a changed byte of code, and a different build, do not. `just release-test` repeats this on synthetic binaries (thin and fat structure, differing signature sizes, page sizes and load command positions) and exercises the refusals.

**Re-running, and the test overrides.** A second `just release-sign-macos` on a release it already signed (`macos=signed` in `verified`) stops, because it would notarize again for an equivalent result; `--force` signs again. The test-only overrides (`FIL_NOTARYTOOL`, `FIL_GH`, `FIL_CODESIGN`, ...) are refused unless the self-test's `FIL_RELEASE_SELFTEST=1` is set, so a real signing and upload cannot run with a faked step. Not covered: a signature blob altered inside the signature region is not seen by the normalizer (by design: it is cut off); `codesign --verify` on a Mac, and the release-key signature over the archive's bytes, are what catch that.

**Stapling.** A bare command-line binary cannot be stapled, so macOS checks Apple's service online the first time it runs.

**What was run for real (ad hoc identity, no Developer ID on the build Mac).** The normalizer on the binaries above, `codesign` and `ditto` on them, and the packing in Docker. The notarization itself (Apple's service, the real certificate) has not been run; the first run is the owner's, on a draft, and changes nothing but that draft.

### 10.4 The .rpm and .deb packages: signed on the owner's Mac

**Decided (owner 2026-11-04, D3):** the packages are signed, and the signing key never lives in CI (N4), as for notarization (§10.3). **Everything below is the design, proposed by PKG;** `just release-sign-packages` (`tests/release/sign-packages.sh`) is built, tested on fakes in `just release-test`, and was run end to end on the rpms of a dry run's Linux binaries with a throwaway key in a Fedora 45 container (rpm 6.1.0, gpg 2.4.9): each signed rpm's body equalled that of an independent unsigned build, and the signed arm64 rpm installed with its signature checked (`install-packages.sh --key`), while the unsigned one was refused.

- **What is signed, and why only that.**
  - **The two .rpm files get an embedded signature** (`rpmsign --addsign`, an OpenPGP signature over the rpm's header, which carries the payload's digest). rpm and dnf check it on install once the key is imported (`rpm --import`; dnf asks for it, or the user imports it first), and Fedora's rpm refuses an unsigned package outright without `--nosignature`.
  - **The .deb files are not signed.** dpkg and apt never verify a signature inside a .deb (`debsig-verify` is not installed or enforced anywhere by default); apt verifies only the signed `Release` file of a repository, and fil has no apt repository. A .deb's integrity is `SHA256SUMS` with its two signatures. A signed apt repository (on the site, say) is possible later and would reuse the package key.
  - **`SHA256SUMS.asc`:** an OpenPGP detached signature over the final `SHA256SUMS`, by the same key, so that a user who has only gpg can verify every file of the release, the .debs included. `SHA256SUMS.sig` (the release key, SSH) stays the release's primary signature.
- **The key.** One OpenPGP key, used only for this: RSA 4096, sign-only, with an expiry date. RSA, not Ed25519, because every rpm that fil's users run verifies RSA (the rpm 4.14 of RHEL 8 and openSUSE Leap 15 does not verify EdDSA; Fedora's own keys are RSA 4096). rpm 6.1's `--addsign` writes both its OpenPGP signature (tag 278) and the v4 RSA header signature (tag 268) that rpm 4.14 reads: checked on 2026-11-04, a package signed by Fedora 45's rpmsign verified with `rpm -K` ("Header V4 RSA/SHA512 Signature: OK") in a Rocky Linux 8 container (rpm 4.14.3). `pkg_inspect.py` counts an rpm as signed only with the v4 header signature, so `sign-packages.sh` refuses a signing that drops it. Its user ID carries no e-mail address, because the public key is committed to this repository.
- **Where the public key lives:** `.github/package-signing-key.asc` (armored), reviewed by a code owner (`.github/CODEOWNERS`), and read from **main** by `release-verify`, never from a tag, like `.github/release-signers`. Its fingerprint is published outside the repository: in the README and on the site's install page, where users get the `rpm --import` command. Until the file holds a key it is a placeholder, and `release-verify` fails after signing.
- **The tooling on the Mac:** Homebrew's `gnupg` (2.5) and `rpm` (6.1.0, which ships `rpmsign` and `rpmkeys`): `brew install gnupg rpm`. **Not yet tried on the Mac;** the flow was checked in Fedora's rpm 6.1.0.
- **The signing step** (`just release-sign-packages X.Y.Z`, step 4 of §9.2), on the files that `release-verify --pre-sign` verified:
  1. it refuses files that do not match their `SHA256SUMS`, and a key file with no key;
  2. for each rpm: `rpmsign --addsign` with the key's fingerprint (gpg asks for the passphrase), then it requires that the rpm now carries a signature, that **everything after the signature header is byte for byte what it was** (the "body": the header and the payload; `pkg_inspect.py`), and that `rpmkeys` verifies the signature with the repository's key in a scratch database;
  3. it rewrites `SHA256SUMS` (the same nine files, sorted, §4.2) and writes `SHA256SUMS.asc`, and checks it with the key.

  Then the owner signs the new `SHA256SUMS` with the release key and uploads the two rpms, `SHA256SUMS`, `SHA256SUMS.asc` and `SHA256SUMS.sig` to the draft with `--clobber` (§9.2, step 4).
- **Reproducibility stays verifiable,** as for the signed macOS binary (§10.3): strip the signature and compare. rpm's signing changes only the signature header, so `release-verify` compares the released rpm's body (everything after the signature header) with the rebuilt, unsigned rpm's body: identical means the signed rpm is the reproducible build plus a signature. After signing, `release-verify` also requires both rpm signatures to verify (`rpmkeys --checksig`) and `SHA256SUMS.asc` to verify (gpg) against the key on main, and it skips the rpms' attestations (of the unsigned bytes) with a note. `release-check` reads signed and unsigned rpms alike and reports which.
- **The owner's key steps** (once, before the first tag; then a PR adds the public key):

  ```sh
  brew install gnupg rpm
  # 1. The key: RSA 4096, sign only, two years; gpg asks for a passphrase (use a long one).
  gpg --quick-generate-key "fil package signing key (Anthony Wertz)" rsa4096 sign 2y
  gpg --list-secret-keys --with-colons "fil package signing key" | awk -F: '$1 == "fpr" { print $10; exit }'   # FPR
  # 2. Backups, offline (an encrypted USB stick, say): the secret key and the revocation
  #    certificate gpg wrote to ~/.gnupg/openpgp-revocs.d/FPR.rev.
  gpg --armor --export-secret-keys FPR > fil-package-key.secret.asc
  cp ~/.gnupg/openpgp-revocs.d/FPR.rev fil-package-key.rev
  # 3. The public key, for the PR to main (and its fingerprint for the README and the site).
  gpg --armor --export FPR > .github/package-signing-key.asc
  gpg --show-keys --with-fingerprint .github/package-signing-key.asc
  # 4. The check: sign a scratch file and verify it with the exported public key only.
  echo test > /tmp/fil-key-check && gpg --local-user FPR --armor --detach-sign /tmp/fil-key-check
  gpg --no-default-keyring --keyring /tmp/fil-key-check.kbx --import .github/package-signing-key.asc
  gpg --no-default-keyring --keyring /tmp/fil-key-check.kbx --verify /tmp/fil-key-check.asc /tmp/fil-key-check
  rm -f /tmp/fil-key-check /tmp/fil-key-check.asc /tmp/fil-key-check.kbx
  ```

  Extend the expiry before it lapses (`gpg --quick-set-expire FPR 2y`), then re-export the public key and send it in a PR: rpm checks a signature against the key's validity, so users re-import the extended key (`rpm --import`). If the secret key is lost or exposed, publish the revocation certificate in a PR (`gpg --import` the `.rev`, re-export) and make a new key.
