# Getting fil onto a remote host

> **Status: M4 package U1** (`docs/m4/plan.md` §3 U1). fil gets onto a host by hand (section 1) or through fil: with `fil install HOST`, or when the first attach finds no fil and the user says yes (section 3). The owner approved the second fixed remote command, the constant install bootstrap, and the install directory `~/.local/share/fil` (M4 OQ-1, 2026-11-04), and every build carries them. On 2026-11-09 the owner made the bootstrap the first command of `remote = "auto"` and added its `PATH` step (section 3.3). The user guide (`docs/user/install.md` and `troubleshooting.md`) has the user-facing parts of this page; this page keeps the mechanism and the tests.

## 1. By hand

fil runs on the remote host as `fil server --stdio`, which ssh hands to your login shell on the host. That shell looks the command up on its `PATH`, and the `PATH` of a non-interactive ssh login is often shorter than the one you see when you log in.

1. Download the release archive for the host (`fil-<ver>-x86_64-linux-musl.tar.gz` or `fil-<ver>-aarch64-linux-musl.tar.gz`; a Mac takes the universal archive), check it against `SHA256SUMS` and its signature (`docs/m4/release.md` §4), and unpack it. Or take `libexec/fil/remote/<arch>-linux/fil` from the archive you installed locally: every archive carries both Linux binaries.
2. Copy `bin/fil` to the host, **as a file, not a symbolic link**, into a directory on the non-interactive `PATH`:

   ```sh
   ssh devbox 'echo $PATH'            # the PATH that fil's remote command will see
   scp fil devbox:/usr/local/bin/fil  # or any directory in that list
   ```

   To use `~/.local/bin` instead, add it to the `PATH` that your shell sets for non-interactive logins: in `~/.bashrc` above its "if not interactive, return" line, in `~/.zshenv`, or in `~/.config/fish/config.fish`.
3. Check: `ssh devbox fil --version` prints `fil <ver> (wire <major>.<rev>)`, the same wire as your local `fil --version`.

With `remote = "plain"` (section 2), or behind a forced command, if the host has no usable `fil`, the first `fil devbox` (or `fil ls devbox`) ends with:

```
fil: fil is not installed on the remote host: the remote command `fil server --stdio` found no `fil` on the PATH of a non-interactive ssh login there (`ssh HOST 'echo $PATH'` shows that PATH). Copy a release's bin/fil into a directory on it, such as /usr/local/bin, or add its directory to the PATH that your shell sets for non-interactive logins (~/.bashrc above its interactive check, ~/.zshenv, ~/.config/fish/config.fish), then run fil again
```

The login shells say a missing command differently (status 127 from sh, bash, zsh, dash, ksh and fish; status 1 and "Command not found." from csh and tcsh); fil recognizes both, and only when the pipe ended before fil's preamble (a relay that exits 127 after it is not "not installed"). The shell's own line (`bash: line 1: fil: command not found` and the like, `fil_transport::pipe::says_fil_is_missing`) is never shown; what follows it in that ssh process is held back until the pipe's outcome is known: shown if fil then starts (an rc file ran a missing `fil`), and otherwise captured, not shown, with fil's own explanation in its place (the held report is added to it when it does not explain a missing fil). ssh's stderr before that line, and its prompts on the terminal, are live as before. With the default `remote = "auto"` the bootstrap runs instead (section 3.3), and its words name `fil install` as well.

A host whose fil speaks another wire says so instead, and which side to upgrade (`DESIGN.md` §5.3).

## 2. The config keys

In `~/.config/fil/config.toml`, per host or under `[defaults]` (`DESIGN.md` §14):

| Key | Values | What it does |
|---|---|---|
| `remote` | `auto` (default), `plain`, `installed` | `auto`: the bootstrap first, and `fil server --stdio` once if a forced command answers it; `plain`: only `fil server --stdio` (for hosts behind a forced-command gate, saving the bootstrap's login there); `installed`: the bootstrap only, never the plain command (a forced command ends the run); like `auto` it starts a fil of this wire on `PATH`, which the bootstrap's text does for every mode; kept for configs that name it |
| `install` | `ask` (default), `never`, `always` | what an interactive first attach does when the bootstrap finds no usable fil: ask once, before the terminal goes raw; only say how to install; install without asking |

The defaults are U1's, set with OQ-1's yes (`docs/m4/plan.md` §7, the proposed calls), not CFG's. A custom `pipe` is never rewritten: it runs as configured, whatever `remote` says, and is never offered an install.

## 3. Through fil

What exists, and what holds it to the approved text:

- **The bootstrap** is `fil_transport::pipe::PipeCommand::BOOTSTRAP`, a compile-time constant (`crates/fil-transport/src/pipe/command.rs`). Compile-time checks refuse a build whose bootstrap is not one line of printable ASCII, has a `'`, `\` or `!` inside its quotes, or holds a version or wire outside the install directory's name pattern. `crates/fil-transport/src/pipe/remote-commands.txt` pins its bytes (for the example 0.1.0 on wire 1.12), and a test keeps it equal, byte for byte, to the text in `docs/m4/plan.md` §7 that the owner approved (M4 OQ-1, 2026-11-04, with the dash fix and N6; the `PATH` step, 2026-11-09).
- **In every build, and nothing else.** The one function that puts it on a command line is `PipeCommand::bootstrap`, which takes a host and nothing else; its callers are `fil install` (`crates/fil/src/install/{boot,client}.rs`) and the run's connector (`crates/fil/src/connect.rs`), which builds it once per run beside the plain command. A background run's ssh options are fixed in `PipeCommand::for_background` (`BatchMode=yes`, `ConnectTimeout=10`, `LogLevel=ERROR`, each unless the command line's own options set it) and go in front of the host, never into the remote part; no caller can add other words to a bootstrap command. What each check proves:
  - the golden and `the_bootstrap_is_the_text_the_owner_approved` pin the script's text;
  - `crates/fil/tests/remote_commands.rs` runs every subcommand that reaches ssh (the attach, `ls`, `kill`, `detach`, `ports`, `forward`, `install`, `--check`, `--remove`) with a recording fake `ssh` first on `PATH`, and requires each remote part to be exactly `fil server --stdio` or this build's bootstrap: the behavioral check, whatever way a command line is built. Against a live fake host (this build's own `fil`, run as the host's login shell would run the remote part) it also covers a live session: a background reconnect after the relay is killed, a takeover and its taking back, `fil ports` and `~#`; and it allows exactly fil's ssh options (`-T`, `-o ServerAlive*=<digits>`, and a background run's `BatchMode=yes`, `ConnectTimeout=10`, `LogLevel=ERROR`, all three or none);
  - `just release-check` requires every `sh -c` in a release binary to be the start of this exact text, for that release's version and wire (`tests/release/check.sh`, with mutations in `tests/release/selftest.sh`): a tripwire for a second shell-wrapped literal, which cannot see a command built from separate argv words.
- **In the run.** `crates/fil/src/install/fallback.rs` (`Run`, one per run, in memory) picks each connection's command; the pipe (`fil_transport::pipe::PipeLink::start`) writes the bootstrap's mode line, `attach`, a constant, and stops at its greeting; `crates/fil/src/install/attach.rs` makes the offer and the upload (section 3.3).
- **What runs nothing new on a host, and is built regardless:** `fil server --install-finish` and `fil server --install-gc` (only the bootstrap runs them), garbage collection by the FROZEN contract of `docs/m4/release.md` §6 (`crates/fil/tests/golden/install-gc.txt`), the three-state lock probe, the choice and checks of the binary to send (the bundle and its `SHA256SUMS`, the release-build mark `FIL_RELEASE_BUILD=1` at compile time, the scan for `FIL_TEST_` and builder home paths).

### 3.1 The exchange

```mermaid
sequenceDiagram
  participant C as fil (client)
  participant B as bootstrap (sh)
  participant F as uploaded fil
  C->>B: attach
  alt this version, ~/.local/bin/fil of this wire (both checked), or a PATH fil of this wire
    B-->>C: FIL-BOOT1 exec
    B->>F: exec fil server --stdio
  else no usable copy
    B-->>C: FIL-BOOT1 attach <uname -s> <uname -m>
    C->>B: put <size> <sha256>, then the bytes
    Note over B: hash tool, existing dirs checked, missing ones made 0700,<br/>bytes into a fresh .new-$$ under umask 077, size and SHA-256 checked
    B->>F: exec fil server --install-finish <ver-wire> <sha256> <created>
    Note over F: chain and hash checked again, renamed into place,<br/>stamped, old copies collected
    F-->>C: FIL-GC ..., FIL-BOOT1 ok <wire>
    C->>F: done (fil install) or go (an attach)
  end
```

A preamble with no `FIL-BOOT1 exec` before it means a forced command ran `fil server --stdio` in place of the bootstrap: the client ends the run at once. `check` lists the installed copies, the `fil` on `PATH` and `~/.local/bin/fil` (all shown sanitized), and ends with `FIL-CHECK-END`; `remove` hands over to `fil server --install-gc` of this exact version only, or answers `FIL-BOOT1 none`.

### 3.2 `fil install`

```
fil install HOST                 install this version into ~/.local/share/fil on HOST
fil install --binary FILE HOST   send FILE (it must fit the host and pass the scan)
fil install --check HOST         what is installed, and the fil on PATH
fil install --remove HOST        remove what fil installed, except a running daemon's copy
```

**Refused before any login:** a host whose config names its own `pipe` (a custom pipe is never rewritten), and a host with `remote = "plain"` (there fil runs only `fil server --stdio`, DESIGN §8.2).

**Time limits, the login included** (a login may wait for a password, and the client cannot tell when it ends): `--check` and `--remove` 120 s, an install 30 minutes (the upload is at most 128 MiB, about 18 minutes at 1 Mbit/s). A run that ends with ssh's exit status other than 0 is a failure, even when its output looked complete. `--check`'s report ends with one fixed line, `FIL-CHECK-END` (the owner's N6, 2026-11-04): a report without it is "no answer from the host", an error, never "nothing installed". So a forced command (`ForceCommand`, or `command=` in `authorized_keys`) that prints nothing and exits 0, such as `/bin/true`, fails `--check` with that message.

**A home mounted `noexec`:** the upload arrives and passes its checks, but the script cannot run it (`exec` fails, and POSIX `sh` exits there, before `undo`). The client says the home may be read-only or mounted noexec; the `.new-*` directory stays until garbage collection removes it after 24 hours, with any parents this run made (`tests/realnet/install.sh` `i_noexec`).

**A host or ssh alias named `install`** is attached with `fil -- install` now that `install` is a subcommand, as for the other subcommand names.

A host where the bootstrap's `attach` already finds a fil of this wire (the install directory, `~/.local/bin/fil`, or the `PATH`) gets nothing: "a fil of wire … is already there". No config line is needed after an install: `remote = "auto"` runs the bootstrap first, which starts the installed copy. fil keeps no state about the host.

Which binary is sent: `--binary FILE` if given (the plan lists it last; a file named on the command line is what the user means), otherwise the bundle beside this executable for the host's OS and architecture (`docs/m4/release.md` §4.3), checked against its `SHA256SUMS`, otherwise this executable if it is a marked release build. Each must fit the host (an ELF of its Linux architecture, or a Mach-O with its CPU) and pass the scan. A host whose config names its own `pipe` is refused: a custom pipe is never rewritten.

### 3.3 In an attach and the other commands

With `remote = "auto"` (the default), every run's connections use the bootstrap first (owner, 2026-11-09). Its `attach` mode tries, in order, this build's exact version directory in `~/.local/share/fil`, `~/.local/bin/fil` (both with their directory checks), and the `fil` on the login's `PATH` (`command -v fil`, which is what `fil server --stdio` would run); the last two only if their `--version` names this build's wire. Otherwise it greets. So the common case is one ssh login, whatever kind of copy the host has, and a host whose fil lives only in `~/.local/share/fil` never prints a shell's `fil: command not found` (an interactive first run shows ssh's stderr as it arrives, so its prompts and errors show).

- **A forced command** (`ForceCommand`, `command=` in `authorized_keys`) runs `fil server --stdio` in place of the bootstrap: its preamble with no `FIL-BOOT1 exec` before it (`PipeErrorKind::Forced`). The run then retries once with the plain command, if nothing has worked in the run yet, and keeps it for its later connections. That costs two logins on such hosts, which `remote = "plain"` saves.
- **The run remembers** which command worked, in memory only (`install::fallback::Run`). Nothing about the host is written on the laptop (DESIGN §8.2).
- **Versions:** only an exact version directory or a fil of this wire is ever started; a different wire is refused at the hello. A fil of another wire on `PATH` is skipped, and the greeting follows.
- **A plain try that finds no fil** (behind a forced command, or `remote = "plain"`): the shell's own line is never shown, and fil's message says what to do (section 1), with the held report when it does not explain a missing fil. What follows the line is held only until the outcome is known (section 1); ssh's stderr before it, and its prompts on the terminal, are live.

What the bootstrap's answer means for the run:

- **`FIL-BOOT1 exec`**: an installed copy (or `~/.local/bin/fil` or the `PATH` fil, of this wire) starts; the run goes on as on the plain command.
- **The greeting** (no usable copy): only the **interactive first attach** may install (one pure decision, `install::attach::decide`, unit-tested over every combination). With `install = "ask"` it asks once on the terminal, before raw mode (`fil: fil is not installed on HOST. Install fil <ver> (wire <w>) into ~/.local/share/fil there? [y/N]`), and only if stdin is a terminal; `always` installs without asking; `never` says how to install. A background reconnect never installs, nor does `fil ls` or another management command: they answer `end` and say how to install. The upload is `fil install`'s (section 3.2's binary choice, without `--binary`), answered with `go`, so the install login becomes the attach.
- **An unmarked preamble** (a forced command ran `fil server --stdio` in place of the bootstrap): that run ends at once, with nothing more sent. With `auto` the plain command follows (above); with `remote = "installed"` the run fails and says to use `"auto"` or `"plain"` for that host.

## 4. Tests

| Where | What |
|---|---|
| `crates/fil-transport/src/pipe/command.rs` | the golden, the equality with the approved text, the line rules, the umask, `LC_ALL`, `echo` and `--version </dev/null` checks |
| `crates/fil/tests/install_gc.rs` | the FROZEN contract of `docs/m4/release.md` §6.5, items 1 to 6, with its golden |
| `crates/fil/tests/install_shells.rs` | the constant itself under every login shell found (sh, bash, zsh, dash, ksh, mksh, tcsh, csh, fish, busybox), each with `/bin/sh` and dash as the inner `sh`, in a scratch `HOME`, against a fake fil: 27 cases each (the `PATH` step, its wire check and the order among the three copies included) |
| `crates/fil/tests/remote_commands.rs` | every subcommand that reaches ssh, run as a process against a recording fake `ssh`: each remote part is exactly `fil server --stdio` or this build's bootstrap, with fil's own options only; `remote = "plain"` refuses `fil install` before any login; a failed login (ssh exits 255, nothing printed) fails every install mode. Against a live fake host: a session's background reconnect, takeover and take-back, `fil ports` and `~#`, all on the bootstrap (its `PATH` step, one login); the bootstrap's fast path to an installed copy on one login, with no "command not found" on the terminal, remembered for the reconnect, `remote = "plain"` and `"installed"`; a forced command with `remote = "installed"` (ends at once) and with `auto` (the plain command once, kept for the reconnect); a plain try behind a forced command that finds no fil (the shell's line never shown); the offer (no, and yes with a development build, which is never sent), `install = "never"`, none of which change the host; `fil ls` on a terminal under `ask` and `always` (never asks or installs); a reconnect that finds the copy gone under `always` (never installs); a fil of another wire on `PATH`, a major apart and a revision apart: skipped by the bootstrap, which starts the installed copy on one login, for an attach and for `fil ls`, and no second login when the installed copy is of that wire too; the session's umask on the `PATH` step and the fast path; a silent host (`/bin/true`) for every install mode |
| `crates/fil-transport/src/pipe.rs`, `pipe/answer.rs`, `pipe/command.rs` (tests) | the bootstrap's run: the mode line first and only for the bootstrap, the greeting handed over and `end` on a decline, an unmarked preamble, the fast path on to the preamble, "ended before the preamble" exactly, the noise bound; the one classifier of the answer (markers end their line, rc noise without a newline before one is skipped, the greeting's names cut to 64 characters), shared with `fil install`; a background run's fixed options, before the host only |
| `crates/fil/tests/install.rs` | the real fil of the build as the upload: install, the fast path, `go`, `check`, `remove`, two installs at once with a collection during an upload, a client killed during an upload (the script sees a short upload and removes what it made), a development build refused; a missing fil under sh and tcsh worded as "not installed" |
| `tests/realnet/install.sh` | real OpenSSH, with bash, dash, zsh, fish and tcsh logins and a stock Alpine host (busybox): the manual-install message (`remote = "plain"`) and the bootstrap's words (`auto`, no shell line), install, check and remove per shell, a forced-command gate, the bundle end to end and a tampered bundle; and the real client on a terminal: the offer answered yes (the install login becomes the session, the next attach takes the fast path), the session's umask the same (027) from the bootstrap's `PATH` step, the fast path and an install, the forced-command gate with `remote = "installed"`, a `noexec` home |
