# The release contract

> **Status: written by M4's package R0 (2026-11-04) from `docs/m4/plan.md` §2.** This is the reference that the release workflow (REL), the installer (U1), the packaging (PKG), the notices (LIC) and the docs (DOC) build on and cite. `docs/m4/plan.md` says why; this file says what. If the two disagree, this file wins for the values below, and the plan is corrected. Items marked **proposed** are the coordinator's calls, which review and the owner may change. Items marked **FROZEN** are a contract between versions of fil: once a release carries them, they change only by the rule in "Changing a frozen item". Everything about the remote install layout and the `--install-gc` contract rested on the owner's answer to M4 OQ-1, which is yes (owner, 2026-11-04).
>
> `DESIGN.md` stays the source of truth. A package that changes a decision here updates this file and `DESIGN.md` in the same PR.

Examples use package version `0.1.0` and wire `1.12`. The wire on `main` at the time of writing is `1.11`; the release carries whatever `PROTOCOL_MAJOR`.`PROTOCOL_REVISION` is on its tag.

## 1. Versioning and the wire

- **The package version** is SemVer, in `[workspace.package] version` in the root `Cargo.toml`. It is the one version of every crate. The first release is **0.1.0**: fil is new, and 0.x tells users that the config and the CLI may still change.
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

Each release carries exactly these files:

| File | What |
|---|---|
| `fil-<ver>-x86_64-linux-musl.tar.gz` | the Linux x86_64 archive |
| `fil-<ver>-aarch64-linux-musl.tar.gz` | the Linux aarch64 archive |
| `fil-<ver>-universal-apple-darwin.tar.gz` | the macOS archive |
| `fil-<ver>-source.tar.gz` | the tagged tree with every dependency vendored (`cargo vendor`), which builds offline. It is the Corresponding Source for the binaries (LIC) |
| `SHA256SUMS` | the checksums of the four files above it: the three binary archives and the source archive |
| `SHA256SUMS.sig` | an SSH signature over `SHA256SUMS`, made with `ssh-keygen -Y sign -n fil-release` by the release identity, the owner's own SSH key (M4 OQ-2; decided, owner 2026-11-04) |

The GitHub build-provenance attestation for each archive is attached to the release by the workflow. It is not a file in the list.

### 4.2 `SHA256SUMS`

- **Format:** the output of GNU `sha256sum`: 64 lowercase hex digits, two spaces, then the file name with no directory part, one line per file, LF line endings.
- **Order:** sorted by file name, bytewise (`LC_ALL=C sort`).
- **Coverage:** exactly four files: the three binary archives and the source archive of §4.1. Not `SHA256SUMS` or `SHA256SUMS.sig`.
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
- **Pinned tools:** the tools that generate content are pinned by version and installed the way `linux-check` installs its tools (install-action with a version): `cargo-about` (LIC) and pandoc (DOC).
- **The Linux binaries and archives must reproduce:** the two CI builds of a candidate give identical SHA-256s, and the owner's independent rebuild (`just release-verify`, in the VM) reproduces the binaries. macOS is best effort.

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

LIC fills in this section: how `THIRD-PARTY-NOTICES.md` is generated and what it covers, and the Corresponding Source offer (the vendored source archive of §4.1).

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

- **A tag push `v*`** builds the files of §4.1 and opens a **draft** GitHub Release with the five files (the four archives and `SHA256SUMS`) and a build-provenance attestation for each archive. It never publishes. **A dry run** (`gh workflow run release.yml --ref BRANCH`) does the same builds and checks, uploads the files to the run (artifact `release-files`) and does nothing else: its jobs have no `id-token` or `attestations` permission.
- **Jobs:** `prepare` (version, `SOURCE_DATE_EPOCH`, the tag checks, `release-deps`, `release-test`), `build-linux` (each Linux target twice, in two jobs), `build-macos` (twice, best effort), `package` (identical-binaries gate, archives assembled twice in the pinned image, identical-archives gate, `release-check`), `smoke-macos` (runs the macOS archive and `release-check` on macOS) and `publish` (tag push only: the only job with `contents: write`, `id-token: write` and `attestations: write`).
- **The tag checks in `prepare` are not the gate.** On a tag push Actions runs the workflow file of the tagged commit, so whoever can push a `v*` tag controls it. The gates are the tag ruleset (OQ-2), the draft that a person publishes and the owner's `release-verify` and signature.
- **A release needs the complete layout.** `release-check --complete` (what a tag push runs) fails until every file of §4.3 is in the archives. A dry run lists the files that LIC and DOC have yet to add as pending. LIC and DOC add their files by producing them into a directory laid out like an archive and passing it to `package.sh --extra`.
- **Version and tag:** the archive names carry the version in `Cargo.toml`; a dry run of a branch is named after it (`0.0.0` before the first release). `release-check` (non-dry) requires the tag to be `v<version>`, a `CHANGELOG.md` section for it and, for a patch release, the wire of the previous tag of the same minor.
- **Paths.** `trim-paths` is not stable in the pinned cargo, so the builds pass `--remap-path-prefix` flags (`tests/release/common.sh`): the workspace becomes `/build`, `CARGO_HOME` `/cargo-home` and the rustup home `/rustup-home`. `release-check` requires `/cargo-home/registry/src/` to appear in each binary, as proof that the remap was applied. When the pinned cargo stabilizes `trim-paths`, set it in `[profile.release]` and drop the flags.

### 9.2 The owner's steps for one release

**Repository settings** (the `v*` tag ruleset that only the owner passes, immutable releases, the code-owner review below, and the others of `docs/m4/plan.md` §5): the owner sets them before the first tag (owner, 2026-11-04).

**The release key.** It is a **dedicated** ed25519 key on the owner's account, used for nothing else, never the agent seat's `~/.ssh/id_ed25519` (which signs agent commits globally). Give it a passphrase (or use an `ed25519-sk` hardware key) and keep a backup. The signing commands below name it explicitly, so a global signing key is never used by accident. The key never leaves the owner's machine, and CI has no signing key and no secret.

0. **Once: the key goes into the repository.** Before the first release, a PR adds the key's public half to `.github/release-signers` on `main`, two lines (`namespaces="git"` for the tag and `namespaces="fil-release"` for the checksums; the file shows the form), reviewed by a code owner (`.github/CODEOWNERS`, with the "require review from Code Owners" ruleset setting). Until then the file is a placeholder with no key: `release-verify` stops at the tag, and the workflow warns. Publish the key's fingerprint out of band (README, site) so that verifiers can check `.github/release-signers` against it; a verifier may keep its own copy and name it in `FIL_RELEASE_SIGNERS`.
1. **Candidate.** Run the dry run from `main` (`gh workflow run release.yml --ref main`), then rebuild it in the VM: `just release-verify VERSION --dry-run --from-run RUN --ref SHA` (a commit SHA; §9.3). Nothing from the download is run on the owner's machine in this mode either.
2. **Tag.** On a clean `main` at the release commit, with the version and `CHANGELOG.md` already merged, make an annotated tag signed with the release key and push that one tag:

   ```sh
   git -c gpg.format=ssh -c user.signingkey=~/.ssh/fil-release.pub tag -s vX.Y.Z -m "fil X.Y.Z"
   git push origin vX.Y.Z
   ```

   The workflow builds the draft. If the `publish` job has to be re-run, delete the stale draft first, or there will be two.
3. **Verify, before signing.** `just release-verify X.Y.Z --pre-sign`. It downloads the draft and **stops at the first failure of its first three steps**, before looking at, let alone running, anything in it: the tag against `.github/release-signers` **on main** (annotated, signed, on main; its commit is resolved once and every later step is bound to that commit), `SHA256SUMS` against the files, and the attestations (`gh attestation verify` pinned to the repository, the workflow identity `…/release.yml@refs/tags/vX.Y.Z`, the commit as source and signer digest, and no self-hosted runner). `--pre-sign` skips only the signature, which does not exist yet. Then it runs `release-check --tag vX.Y.Z --complete --no-exec` (the files are unpacked, hashed, grepped and inspected; nothing from the download is run: no `cargo` in the source archive, no `--version` of a binary) and rebuilds the Linux binaries and the four archives from that commit in the VM and compares hashes with the draft's. The macOS archive is only *reassembled* around the released macOS binary, which is not rebuilt.
4. **Sign the verified file.** Sign the copy that step 3 verified (`target/release-verify/X.Y.Z/files/SHA256SUMS`), not a fresh download, because any writer can replace draft assets in between:

   ```sh
   ssh-keygen -Y sign -f ~/.ssh/fil-release -n fil-release target/release-verify/X.Y.Z/files/SHA256SUMS
   gh release upload vX.Y.Z target/release-verify/X.Y.Z/files/SHA256SUMS.sig
   just release-verify X.Y.Z                       # now the signature is checked too
   gh release edit vX.Y.Z --draft=false            # publish
   just release-verify X.Y.Z --no-rebuild          # once more, on what is published
   ```

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
