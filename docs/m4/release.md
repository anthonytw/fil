# The release contract

> **Status: written by M4's package R0 (2026-11-04) from `docs/m4/plan.md` §2.** This is the reference that the release workflow (REL), the installer (U1), the packaging (PKG), the notices (LIC) and the docs (DOC) build on and cite. `docs/m4/plan.md` says why; this file says what. If the two disagree, this file wins for the values below, and the plan is corrected. Items marked **proposed** are the coordinator's calls, which review and the owner may change. Items marked **FROZEN** are a contract between versions of fil: once a release carries them, they change only by the rule in "Changing a frozen item". Everything about the remote install layout and the `--install-gc` contract depends on the owner's answer to M4 OQ-1; if the answer is no, no install directory is ever created, and sections 5 and 6 are unused.
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
- **FROZEN: the first line.** Tests, the bootstrap script (which greps the `--version` of a candidate for `(wire <major>.<rev>)`) and the docs parse it. LIC may add GNU-style notice lines after it and nothing before it.
- **`<ver>`** uses only `[0-9A-Za-z.+-]` and starts with a digit. A pre-release is `0.1.0-rc.1`. Build metadata (`+…`) is not used.

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
| `SHA256SUMS` | the checksums of the five files above |
| `SHA256SUMS.sig` | an SSH signature over `SHA256SUMS`, made with `ssh-keygen -Y sign -n fil-release` by the release identity (OQ-2) |

The GitHub build-provenance attestation for each archive is attached to the release by the workflow. It is not a file in the list.

### 4.2 `SHA256SUMS`

- **Format:** the output of GNU `sha256sum`: 64 lowercase hex digits, two spaces, then the file name with no directory part, one line per file, LF line endings.
- **Order:** sorted by file name, bytewise (`LC_ALL=C sort`).
- **Coverage:** the five files of §4.1 above it (the four archives and the source archive), and not `SHA256SUMS` or `SHA256SUMS.sig`.
- **Check:** `sha256sum -c SHA256SUMS` (or `shasum -a 256 -c SHA256SUMS`) in the download directory. The signature is checked with `ssh-keygen -Y verify -n fil-release` against the allowed-signers file from `main`, never from the tag (REL's `release-verify`).

### 4.3 The archive layout

Every archive unpacks to a single top-level directory, named like the archive without `.tar.gz` (for example `fil-0.1.0-aarch64-linux-musl/`). Relative to that directory, every archive has this layout:

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
- **The bundle** is `libexec/fil/remote/`. Every archive carries the static Linux binaries, so any client can install fil on either Linux architecture (U1). The directory names are `x86_64-linux` and `aarch64-linux`: the remote's `uname -s` lowercased, with `uname -m` mapped (`x86_64` to `x86_64-linux`; `aarch64` and `arm64` to `aarch64-linux`).
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

- **The recipe, one for all archives:**
  - entries sorted by name;
  - every mtime set to `SOURCE_DATE_EPOCH`, which is the tagged commit's committer time;
  - `--owner=0 --group=0 --numeric-owner` and fixed modes (directories 0755, executables 0755, other files 0644);
  - `gzip -n`.
- **Pinned tools:** the tools that generate content are pinned by version and installed the way `linux-check` installs its tools (install-action with a version): `cargo-about` (LIC) and pandoc (DOC).
- **The Linux binaries and archives must reproduce:** the two CI builds of a candidate give identical SHA-256s, and the owner's independent rebuild (`just release-verify`, in the VM) reproduces the binaries. macOS is best effort.

## 5. The remote install layout

### 5.1 The directory

- **The path:** `$HOME/.local/share/fil/<ver>-w<major>.<rev>/fil`. For example, `~/.local/share/fil/0.1.0-w1.12/fil`. The version directory is mode 0700 and the file 0700.
- **The name carries the wire**, so garbage collection can tell which daemons still need a copy without running any binary.
- **`$HOME`-based, not `$XDG_DATA_HOME`.** A non-interactive login may see a different environment from an interactive one, and the path must not move between them. The state directory `~/.local/state/fil` follows the same rule.
- **A third place where fil writes on the remote.** `DESIGN.md` §11.1.5 (decided, owner 2026-11-02) says fil writes nothing beyond its state directory and its socket directory. `~/.local/share/fil` is part of what the owner approves in M4 OQ-1; U1 changes §11.1.5 only as OQ-1 decides. (`DESIGN.md` §11, "Server binary distribution", names an earlier form of the path, `~/.local/share/fil/<version>/`. U1 rewrites that paragraph to this section.)
- **Never replaced in place.** A version is installed once, by a rename from a fresh directory. A new version gets a new directory. fil never modifies, replaces or removes a `fil` it did not install, such as one on `PATH` or one from a package manager.
- **Fresh directories:** an upload goes into `~/.local/share/fil/.new-<pid>` (the shell's `$$`), created with `mkdir -m 700` and renamed to its final name only after the size and SHA-256 are verified.
- **Leftovers.** A bootstrap killed mid-upload leaves its `.new-*` directory, which garbage collection removes once it is more than an hour old (§6), and any of `~/.local` and `~/.local/share` (mode 0700) that it had created. Those stay.

### 5.2 What the bootstrap and the client may assume

The bootstrap script (`docs/m4/plan.md` §3, U1; the exact text is M4 OQ-1) builds the directory name from its own constants: `v=<ver>-w<major>.<rev>`, with both parts checked at compile time to use only `[0-9A-Za-z.+-]`. It `exec`s `fil server --stdio` from the exact-version directory only after the checks in the plan. Nothing in this file changes those checks.

## 6. FROZEN: the `--install-gc` contract

`fil server --install-gc` (the `remove` mode of the bootstrap, `fil install --remove HOST`) and the collection that `fil server --install-finish` runs after an install both select what to remove with **one** function, which implements this section. This section fixes what any copy of fil, now or later, may rely on when it judges another copy's directories.

### 6.1 What GC looks at

GC looks at the entries of exactly one directory, `$HOME/.local/share/fil`, and at two files per wire in the socket directory. It lists nothing else, and it never removes `~/.local/share/fil` itself or any of its parents.

### 6.2 The pinned values

| Item | Value |
|---|---|
| Install root | `$HOME/.local/share/fil` (the environment's `$HOME`, which the bootstrap has already checked) |
| Version directory name | matches `^<ver>-w<major>\.<rev>$` with `<ver>` = `[0-9][0-9A-Za-z.+-]{0,63}`, and `<major>` and `<rev>` each `0` or `[1-9][0-9]{0,4}` and at most 65535. The split is at the **last** `-w`. A conforming name has `<major>` and `<rev>` both at least 1 |
| Socket directory | `/tmp/fil-<uid>`, `<uid>` the effective user id in decimal (`DESIGN.md` §5.3) |
| Daemon lock file of a wire | `<socket directory>/fil-<major>-<rev>.lock` |
| Start lock file of a wire | `<socket directory>/fil-<major>-<rev>.start` |
| Fresh-upload directory | an entry named `.new-` followed by one or more characters |
| The `.new-*` age bound | **3600 seconds**: collected only when `now - mtime > 3600 s` of the directory itself. A modification time in the future is not old |

Where `<major>` and `<rev>` in the lock names are the numbers from the version directory's name, in decimal, without leading zeros, as `fil_daemon::paths` writes them. `fil_daemon::paths` (`Paths::lock`, `Paths::start_lock`) defines the lock names the daemon and the relay use today; this section is the same names, stated for the readers of other versions.

### 6.3 The selection

GC works through the entries of the install root and decides each one alone:

1. **An entry whose name matches the version-directory pattern** is a candidate only if it is a real directory (`lstat`: not a symbolic link) owned by the user. Anything else with such a name is left alone.
2. **A candidate is kept if any of these holds,** and removed otherwise:
   1. **It is the current copy:** the directory the running installer has just installed (`--install-finish`'s collection only). `--install-gc` exempts no directory by identity: it is `remove`, and it removes the copy it runs from too (the daemon rule still applies).
   2. **Its wire has a running daemon:** the file `fil-<major>-<rev>.lock` in the socket directory exists and is held. Held means a shared, non-blocking `flock` on the file, opened read-only with `O_NOFOLLOW | O_NONBLOCK`, fails with `EWOULDBLOCK`. This is the probe `fil ls` uses for other daemons: it creates, removes and connects to nothing. All directories of a wire are kept, whatever their `<ver>`.
   3. **Its wire's start lock is held:** the same probe on `fil-<major>-<rev>.start`, so GC never races a relay that is starting a daemon from that directory.
   4. **Unknown is keep:** the probe could not run for a reason other than the file being absent (any `open` or `flock` error other than `ENOENT` and `EWOULDBLOCK`), or the socket directory exists but fails the checks of `DESIGN.md` §5.3 (not a real directory, wrong owner, mode not exactly 0700). A **missing** socket directory or a missing lock file is "no daemon".
3. **An entry named `.new-*`** is collected if it is a real directory (not a symbolic link) owned by the user, and its modification time is more than 3600 seconds in the past. Younger ones are never touched, so GC never removes another client's upload in flight. A process ID alone cannot tell uploads apart: IDs repeat across hosts that share an NFS home, and across containers.
4. **Every other entry** (files, symbolic links, directories with any other name, a `.new` without the dash) is never touched.
5. **Removal** of a selected directory removes its contents recursively without following symbolic links. A failure to remove is reported and does not stop the pass.

GC prints nothing that the user did not ask for beyond its result line; the line's format belongs to U1, not to this contract.

### 6.4 Changing a frozen item

A later version may **extend** collection only so that **an older copy's rules stay safe** for everything that the later version creates. That means:

1. **Names it recognizes: only a subset.** For any entry this section's rules recognize (a conforming version directory or a `.new-*` entry), a later version collects a subset of what §6.3 collects. It may add reasons to keep. It may not remove a directory that §6.3 keeps, and it may not shorten the age bound (a longer bound is fine). A shorter bound would remove the upload in flight of an older client, which relies on the hour.
2. **Names it does not recognize: only its own kinds.** A later version may collect kinds of entries that this section does not recognize, but only entries of kinds it defines and creates itself, and never one whose name matches the version-directory pattern or `.new-*`. Older copies leave such names alone (§6.3, 4).
3. **Everything it installs follows the pattern.** A later version installs its binaries only as `<ver>-w<major>.<rev>` directories, and it stages uploads only in `.new-*` directories that it finishes (renames) in well under an hour.
4. **It keeps the lock files for every wire that has an install directory.** A daemon of wire `<major>.<rev>` holds `fil-<major>-<rev>.lock` for its whole life, and a relay holds `fil-<major>-<rev>.start` while it starts one, in `/tmp/fil-<uid>`, under those names, in every later version, including after a change of `PROTOCOL_MAJOR`. Otherwise an older GC would find "no daemon" and remove a copy that a live daemon runs from. Anything that keeps those files alive is part of the same contract; that is TMP's job (§2.4 of the plan).
5. **A change to the pattern, the file names, the socket directory or the age bound itself** is not an extension. It needs a new install root name (not `~/.local/share/fil`), so that older copies never see the new layout, and it goes to the owner, because it is a new place where fil writes (§5.1).

### 6.5 What the test must pin (U1)

U1 implements `--install-gc` and `--install-finish`'s collection, and writes the test that pins this section. The values are literals in the test, not derived from the code under test, so renaming a file or changing the bound fails it:

1. **The name pattern.**
   - Accepted, with their `(ver, major, rev)`: `0.1.0-w1.12` → (`0.1.0`, 1, 12); `0.1.0-rc.1-w1.12` → (`0.1.0-rc.1`, 1, 12); `1.2.3-w65535.65535`; `0.1.0+b-w1.1`.
   - Refused (the entry is then left alone): `0.1.0`, `0.1.0-w1`, `0.1.0-w1.`, `0.1.0-w.12`, `0.1.0-w01.12`, `0.1.0-w1.012`, `0.1.0-w0.12`, `0.1.0-w1.0`, `0.1.0-w1.65536`, `0.1.0-w1.12.3`, `-w1.12`, `.0.1.0-w1.12`, `0.1.0-w1.12/`, `0.1.0 -w1.12`, `0.1.0-W1.12`, a ver of 65 characters, an empty name.
2. **The lock names.** For `0.1.0-w1.12` and uid 1000: `/tmp/fil-1000/fil-1-12.lock` and `/tmp/fil-1000/fil-1-12.start`. A second assertion checks that `fil_daemon::paths::Paths` produces the same file names for wire 1.12, so the daemon and GC cannot drift apart.
3. **The age bound.** A `.new-*` directory with mtime 3599 s old is kept; 3600 s is kept; 3601 s is collected; one in the future is kept. A `.new-x` that is a symbolic link, a regular file or owned by another user is never touched. `.new`, `new-1` and `x.new-1` are never touched.
4. **Each keep rule alone, and each removal:**
   - a directory whose wire's lock is held (a real `flock` taken by the test) is kept, and another directory of the same wire with a different `<ver>` is kept too;
   - the same with only the start lock held;
   - the install-time current copy is kept with no lock held, and `--install-gc` removes the same directory;
   - a directory whose wire has a lock file that nobody holds, and one with no lock file, are removed;
   - a lock file that is a symbolic link or a FIFO neither hangs nor counts as held, and a socket directory with the wrong owner or mode keeps everything (unknown is keep);
   - a missing socket directory removes the unprotected candidates.
5. **Nothing else.** Files, symbolic links to directories, and directories with unknown names in the install root are untouched, and `~/.local/share/fil` and its parents are never removed, even when the pass empties them.
6. **Mutation table.** The PR's mutation table includes: change the age bound; shorten the lock name; treat a held start lock as "free"; follow a symbolic link at a `.new-*` entry; treat an unreadable lock as "no daemon". Each must fail the named test.

### 6.6 A known consequence

The keep rules protect a copy only while its wire's daemon runs or is starting. A copy that `fil install HOST` has just installed, with no attach yet, is therefore collectable by the next install of any other version on that host: with two laptops that share a home directory and run different versions, each can remove the other's idle copy, and each reinstalls on its next attach. That costs an upload and loses no session, because a session exists only while its daemon runs, and the daemon's wire is protected. It is accepted for 0.1.0. A grace period for freshly installed copies would be a keep reason; by §6.4 a later version can add it, but copies that predate it would still collect without it.

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
| TMP | §6.4, item 4 (the lock files that GC reads) |
