# Install

fil is one program, `fil`, that is both the client on your machine and the program that runs on the host you connect to. You need it in two places: on your machine, and on every host you want to connect to. There is nothing to run as root, and no service to start on the host: fil starts its own daemon, as you, when you attach.

fil runs on macOS and Linux (x86_64 and aarch64) as the client, and on Linux as the host. A Mac also works as a host, with less testing. On Windows, run fil inside WSL, which is a Linux host.

## On your machine

| Channel | Command | What you get |
|---|---|---|
| The installer (from the release after 0.3.0) | `curl -fsSL https://github.com/anthonytw/fil/releases/latest/download/fil-install.sh \| sh` (below) | the client in `~/.local/bin` and the Linux programs `fil install` uploads, checked against the signed release; no root |
| GitHub Releases | download, check, unpack (below) | the client, the Linux programs `fil install` uploads, man pages and completions |
| Homebrew (macOS, Linux) | `brew install anthonytw/tap/fil` | the same archive |
| Debian, Ubuntu | `sudo apt install ./fil_<version>_amd64.deb` (or `_arm64`) | the same files under `/usr` |
| Fedora, RHEL, openSUSE | `sudo rpm --import package-signing-key.asc` (the `.github/package-signing-key.asc` of the repository), then `sudo dnf install ./fil-<version>-1.x86_64.rpm` (or `.aarch64`) | the same |
| fil's apt and dnf repositories (from the release after 0.3.0) | set up once (below), then `sudo apt install fil` or `sudo dnf install fil` | the same packages, upgraded with the rest of the system |
| Arch Linux | `fil-shell-bin` on the AUR | the same (see the release notes for when it is published) |
| crates.io | `cargo install --locked fil-shell` | a build from source; no man pages, completions or Linux programs to upload |
| Nix | `nix profile install github:anthonytw/fil/v<version>` | a build from source, as with cargo |

The command is `fil` everywhere.

### The installer

From the release after 0.3.0, every release carries `fil-install.sh`, a short POSIX shell script that needs no root. It downloads the release's `SHA256SUMS` and `SHA256SUMS.sig` with `curl` (HTTPS only, redirects included), checks the signature with `ssh-keygen -Y verify` against the release key written in the script, picks the archive for your machine from that list, checks its SHA-256, and copies `fil` into `~/.local/bin` and the Linux programs `fil install` uploads into `~/.local/libexec/fil/remote` (if `~/.local/bin` is a symbolic link, into the `libexec` beside the directory it points to, where `fil` looks). If a check or a copy fails it stops with nothing installed, and it runs nothing it downloaded but the installed `fil --version` at the end. It needs `curl`, `ssh-keygen` from OpenSSH 8.1 or later (for `-Y verify`; RHEL 8's OpenSSH 8.0 is too old), `tar`, `gzip`, and `sha256sum` or `shasum`, and checks for each before it downloads anything.

The one-liner, which installs the latest release:

```sh
curl -fsSL https://github.com/anthonytw/fil/releases/latest/download/fil-install.sh | sh
```

Or download it, read it, then run it, for a given version:

```sh
curl -fsSLO https://github.com/anthonytw/fil/releases/download/v<version>/fil-install.sh
less fil-install.sh
sh fil-install.sh --version <version>
```

`--dir DIR` (or `FIL_INSTALL_DIR`) installs into another directory instead of `~/.local/bin`; the Linux programs go to `DIR/../libexec/fil/remote`. The script tells you if the directory is not on your `PATH`. To check the script itself before running it, it is listed in the release's `SHA256SUMS` like every other file (below). Run it again to upgrade.

### fil's apt and dnf repositories

From the release after 0.3.0, fil's site serves an apt and a dnf repository with the release's `.deb` and `.rpm` packages, signed with the package signing key (fingerprint `9EF0 B189 34B7 0552 0F88  80A5 81C0 98D1 5D1A EC26`, also in the [README](../../README.md)). Check the fingerprint `gpg --show-keys` prints before you trust the key.

Debian and Ubuntu:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://fil.anthonywertz.com/apt/fil-package-key.asc | sudo tee /etc/apt/keyrings/fil.asc > /dev/null
gpg --show-keys /etc/apt/keyrings/fil.asc        # 9EF0 B189 34B7 0552 0F88  80A5 81C0 98D1 5D1A EC26
echo "deb [signed-by=/etc/apt/keyrings/fil.asc] https://fil.anthonywertz.com/apt stable main" | sudo tee /etc/apt/sources.list.d/fil.list
sudo apt update && sudo apt install fil
```

Fedora, RHEL and others with dnf:

```sh
sudo curl -fsSL -o /etc/yum.repos.d/fil.repo https://fil.anthonywertz.com/rpm/fil.repo
sudo dnf install fil      # dnf shows the key's fingerprint and asks before it imports it
```

The repository's index (`InRelease` for apt, `repomd.xml.asc` for dnf) is signed, and it lists each package by its SHA-256, so apt and dnf refuse a package or an index that was changed; dnf also checks each rpm's own signature. Each repository holds the latest release only.

The package signing key expires on 2028-11-08 and will be extended before then. apt keeps the copy you downloaded, so after an extension run the `curl … /etc/apt/keyrings/fil.asc` line above again (the fingerprint stays the same); dnf asks to import the extended key on its own. The crate is `fil-shell` and the AUR package `fil-shell-bin` because those names were taken by other projects: `cargo install fil` installs something else.

### Checking a download

Every file of a release is listed in `SHA256SUMS`, which is signed (the installer, `fil-install.sh`, too). The release signing key's fingerprint and the package signing key's are in the [README](../../README.md); compare them with the copies in the repository before you trust a release.

```sh
# with the release signers file from the repository's main branch
ssh-keygen -Y verify -f release-signers -I release -n fil-release -s SHA256SUMS.sig < SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS      # shasum -a 256 -c on a Mac
```

With gpg, import the package signing key and run `gpg --verify SHA256SUMS.asc SHA256SUMS`. The `.rpm` files are signed with the package key, so `rpm` and `dnf` check them once the key is imported; the `.deb` files carry no signature of their own and are covered by `SHA256SUMS`.

### From an archive, by hand

Unpack the archive for your machine (`fil-<version>-x86_64-linux-musl.tar.gz`, `…-aarch64-linux-musl.tar.gz`, or `…-universal-apple-darwin.tar.gz` for a Mac) and copy `bin/fil` into a directory on your `PATH`, for example `~/.local/bin`. Copy it; don't make a symbolic link. Keep `libexec/fil/remote/` where it is relative to the binary (`../libexec/fil/remote` from `bin/`) if you want `fil install` to upload the bundled Linux programs.

The archive's `share/man` and `share/*completion*` directories hold the man pages and completions. Put them where your system looks, or let a package do it. Check the install with:

```sh
fil --version        # fil <version> (wire <major>.<revision>), then how to upgrade this copy
```

## On a host

There are three ways, and fil can do the second and third for you.

### 1. By hand

fil runs on the host as `fil server --stdio`, which ssh hands to your login shell there. The shell looks `fil` up on its `PATH`, and the `PATH` of a non-interactive ssh login is often shorter than the one you see when you log in.

1. Take `bin/fil` from the Linux archive for the host, or `libexec/fil/remote/<arch>-linux/fil` from the archive you installed locally: every archive carries both Linux programs.
2. Copy it to the host as a file, not a symbolic link, into a directory on the non-interactive `PATH`:

   ```sh
   ssh devbox 'echo $PATH'            # the PATH that fil's remote command will see
   scp fil devbox:/usr/local/bin/fil  # or any directory in that list
   ```

   To use `~/.local/bin`, add it to the `PATH` your shell sets for non-interactive logins: in `~/.bashrc` above its "if not interactive, return" line, in `~/.zshenv`, or in `~/.config/fish/config.fish`.
3. Check: `ssh devbox fil --version` prints the same wire as your local `fil --version`.

### 2. `fil install`

```sh
fil install devbox                 # install this version into ~/.local/share/fil on devbox
fil install --binary FILE devbox   # send FILE instead
fil install --check devbox         # show what is installed, and the fil on the PATH; change nothing
fil install --remove devbox        # remove what fil installed, except a copy a running daemon uses
```

`fil install` logs in with ssh, as `fil devbox` would, and sends the right Linux program for the host (chosen from its `uname`) over that login. Nothing is downloaded. Before anything on the host runs, it checks the size and the SHA-256 of what arrived. The copy goes into `~/.local/share/fil/<version>-w<wire>/`, which is private to you. After an install, old copies are cleaned up, except any that a running daemon holds or that were used or changed within the last 7 days.

Which file is sent: the one named with `--binary`; otherwise the bundled program beside your `fil` for the host's system and architecture (checked against the bundle's `SHA256SUMS`); otherwise your own `fil`, if it is a release build for that system. A build from source is never sent; name a release file with `--binary`.

`fil install` refuses a host whose [config](config.md) names its own `pipe`, and a host with `remote = "plain"`. It also refuses a home directory that is itself a symbolic link; see [troubleshooting](troubleshooting.md).

### 3. On the first attach

If you run `fil devbox` and the host has no fil, an interactive first attach asks once, before your terminal goes raw:

```text
fil: fil is not installed on devbox. Install fil <version> (wire <w>) into ~/.local/share/fil there? [y/N]
```

Answer `y` and the install login becomes your session. The `install` setting in your [config](config.md) changes the question: `ask` (the default), `never` (only say how to install), or `always`. A reconnect in the background never installs, and neither do `fil ls` and the other commands.

The `remote` setting in the same file chooses the command fil runs on the host: `auto` (the default) runs the install command first, which starts the copy fil installed, `~/.local/bin/fil` or the `fil` on the host's `PATH` (each only if it speaks your fil's wire), or asks to install, all on one ssh login; `plain` runs only `fil server --stdio`, for hosts behind a forced-command gate; `installed` is like `auto` but never falls back to `fil server --stdio`. fil writes nothing about the host on your machine.

## Upgrading

fil never updates itself and never checks for a new version. `fil --version` says, on its second line, how this copy was installed and the command that upgrades it: `brew upgrade anthonytw/tap/fil`, `cargo install --locked fil-shell`, the new `.deb` or `.rpm` from the release page, your AUR helper for `fil-shell-bin`, your Nix profile or flake, or, for a copy you put somewhere yourself, the release page and its path. It works that out from where the program is and the package manager's files on your machine, without the network and without running anything. On a host, a copy that `fil install` put there is upgraded by connecting with a newer fil, which installs its own beside it. `fil ls` also says when a host's daemon is newer than your fil.

Install the new version on your machine and on each host. Two builds talk to each other only if their wire numbers are equal (the `wire` in `fil --version`). Daemons of different wires run side by side on a host, so an upgrade breaks no running session: your old sessions stay on the old daemon until they end. A patch release (0.1.0 to 0.1.1) never changes the wire, and 0.2.0 kept 0.1.0's (0.3.0 changes it, to `wire 1.15`: upgrade both ends, and a 0.3.0 client starts a 0.3.0 daemon beside the old one). When the wire is unchanged, a new client reuses the running daemon, and daemon-side fixes take effect only once that daemon has exited. End your sessions on each host (or `fil kill` them) to pick up the fixes.

## Removing fil

`fil install --remove HOST` removes the copies fil installed on a host. Sessions in use are left alone until they end; run it again afterwards. `--remove` runs from the copy of **this version** of fil that is installed on the host; with another version of fil it fails and says to run it with that version. fil leaves on the host only its state directory (`~/.local/state/fil`, the logs), its socket directory under `/tmp`, and the empty directories `~/.local/share/fil`, `~/.local/share` and `~/.local` if the install made them. Remove a copy you installed by hand the way you put it there, and delete `~/.config/fil` if you made one.
