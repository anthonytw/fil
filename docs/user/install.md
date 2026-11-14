# Install

fil is one program, `fil`, that is both the client on your machine and the program that runs on the host you connect to. You need it in two places: on your machine, and on every host you want to connect to. There is nothing to run as root, and no service to start on the host: fil starts its own daemon, as you, when you attach.

fil runs on macOS and Linux (x86_64 and aarch64) as the client, and on Linux as the host. A Mac also works as a host, with less testing. On Windows, run fil inside WSL, which is a Linux host.

## On your machine

| Channel | Command | What you get |
|---|---|---|
| GitHub Releases | download, check, unpack (below) | the client, the Linux programs `fil install` uploads, man pages and completions |
| Homebrew (macOS, Linux) | `brew install anthonytw/tap/fil` | the same archive |
| Debian, Ubuntu | `sudo apt install ./fil_<version>_amd64.deb` (or `_arm64`) | the same files under `/usr` |
| Fedora, RHEL, openSUSE | `sudo rpm --import package-signing-key.asc` (the `.github/package-signing-key.asc` of the repository), then `sudo dnf install ./fil-<version>-1.x86_64.rpm` (or `.aarch64`) | the same |
| Arch Linux | `fil-shell-bin` on the AUR | the same (see the release notes for when it is published) |
| crates.io | `cargo install --locked fil-shell` | a build from source; no man pages, completions or Linux programs to upload |
| Nix | `nix profile install github:anthonytw/fil/v<version>` | a build from source, as with cargo |

The command is `fil` everywhere. The crate is `fil-shell` and the AUR package `fil-shell-bin` because those names were taken by other projects: `cargo install fil` installs something else.

### Checking a download

Every file of a release is listed in `SHA256SUMS`, which is signed. The release signing key's fingerprint and the package signing key's are in the [README](../../README.md); compare them with the copies in the repository before you trust a release.

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
fil --version        # fil <version> (wire <major>.<revision>)
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

Install the new version on your machine and on each host. Two builds talk to each other only if their wire numbers are equal (the `wire` in `fil --version`). Daemons of different wires run side by side on a host, so an upgrade breaks no running session: your old sessions stay on the old daemon until they end. A patch release (0.1.0 to 0.1.1) never changes the wire, and 0.2.0 kept 0.1.0's (0.3.0 changes it, to `wire 1.15`: upgrade both ends, and a 0.3.0 client starts a 0.3.0 daemon beside the old one). When the wire is unchanged, a new client reuses the running daemon, and daemon-side fixes take effect only once that daemon has exited. End your sessions on each host (or `fil kill` them) to pick up the fixes.

## Removing fil

`fil install --remove HOST` removes the copies fil installed on a host. Sessions in use are left alone until they end; run it again afterwards. `--remove` runs from the copy of **this version** of fil that is installed on the host; with another version of fil it fails and says to run it with that version. fil leaves on the host only its state directory (`~/.local/state/fil`, the logs), its socket directory under `/tmp`, and the empty directories `~/.local/share/fil`, `~/.local/share` and `~/.local` if the install made them. Remove a copy you installed by hand the way you put it there, and delete `~/.config/fil` if you made one.
