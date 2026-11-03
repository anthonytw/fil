# The drop-in suite (M1 package P8)

The owner's rule (2026-11-02): no human testing in the pipeline. "If you're able to use fil as more or less a drop-in replacement for ssh, et, or mosh, then you're on the right track. Automate whatever you can." This suite is that checkpoint. An agent drives the real `fil` client on a pseudo-terminal and does what people do with ssh, et and mosh: edits files in vim, pages with less, watches top, uses git, recalls history, completes names, stops jobs, floods the screen, resizes the window, detaches and comes back, takes a session over from a second machine, loses its ssh, kills its client, and puts the laptop to sleep. Each scenario asserts that fil behaves at least as well as ssh would. Where it does not, that is a finding (below), not a tolerance.

It is on demand and never part of `just ci`. (Its unit tests, which pin the exact-sequence rules and the target rules, do run in `just ci`.)

## Where it runs

| Where | What | How |
|---|---|---|
| The lab Raspberry Pis (aarch64, Debian 13) | 30 scenarios, this Mac's client against `fil server --stdio` on each Pi through the `fil-test` gate | `crates/fil/examples/dropin/`, `just dropin` |
| The real-network harness (`udp` part, a Linux runner) | vim while the NAT mapping moves and while UDP is lossy and dead; the same everyday battery through `ssh -tt` and through fil, side by side | `tests/realnet/udp.sh` (`f_dropin_*`), helpers in `tests/realnet/dropin.sh` |
| This machine (`--local`) | a development aid, not a result: the scenarios against a **private** daemon (`fil-daemon-dev --dir`), never `/tmp/fil-$UID`, so a stale daemon of another tree cannot answer; it is stopped at the end by the pid in its lock file (never by a pattern) and the run fails if it is still alive | `just dropin --local` |

Every tool runs inside the fil session's shell, which is what a user does. The gate (`fil server --stdio`, `install`, `run`, `status`, `logs`, `reset`) offers no shell, so **plain `ssh -t` cannot be run on the Pis**. The like-for-like comparison with ssh therefore runs in realnet, where the "server" is a real sshd with a real login shell.

The harness (the `FIL_PI_TARGETS` rules, the gate, cleanup and the client on a pty) is one module, `crates/fil/examples/support/harness.rs`, used by both `pi_e2e` and `dropin`. It took over the part of the sweep that would have de-duplicated them.

## How to rerun

```sh
export FIL_PI_TARGETS="pi1=fil-test@HOST1,pi2=fil-test@HOST2"   # required; the keys are the only hosts it may touch
just dropin                                   # both Pis, every scenario (about 12 minutes per Pi, with cleanups)
just dropin --hosts pi1 --only vim_edit,flood # narrow
just dropin --list                            # the scenario names
just dropin --local                           # this machine, a private daemon, no targets needed
```

The Pi binary must be installed first (`docs/m1/pi.md`), and it must be built from the same tree as the client: the wire format moves between milestones, and a client and daemon from different trees stop at "protocol error".

The run is strict by default. It **fails** when:

- a scenario is skipped on a lab host (`--allow-skip` to accept it);
- a required tool is missing (`--require vim,less,top,htop,git` is the default list; `--allow-missing` to accept it): a reimaged Pi must not turn the gate green by quietly dropping the vim scenarios;
- a gate verb cannot run or exits non-zero, including the final `status` (silence is not "clean");
- a known failure starts to pass (XPASS), so its entry is removed with the fix; and a known-failing scenario that fails in any *other* way than its signature (the old size seen; the modes left set) is a failure too.

The summary prints the number of scenarios run, failed, skipped (with their names) and expected failures. Every message the driver prints goes through a scrubber that swaps the real host names for `pi1`/`pi2`, and the shells use a neutral prompt (`fil$ `).

Cleanup is that of `pi_e2e`: `fil kill` for this driver's sessions (all named `dp-*`), then the gate's `reset`, before and after every scenario; each scenario's scratch directory is `/tmp/dp.*` on the Pi, swept at the start of each shell and at the end of a full run.

A driver stopped with Ctrl-C while it holds a client's process groups stopped (the sleep scenarios) leaves them stopped: `kill -CONT -- -PGID` for the groups of the `fil` clients it started. A panic or a failed check continues them (a drop guard).

Realnet: `just realnet udp f_dropin_vim_nat f_dropin_vim_loss f_dropin_vs_ssh` on a Linux box, or the `linux-check` workflow with `-f suite=realnet` (it runs everything; the three scenarios take about one minute each).

## How a scenario checks

- The client runs on a pty (`portable-pty`). What it prints goes through a `vt100` emulator (what a terminal would show, including the alternate screen, wide cells, colors, attributes and the terminal modes) and a stream with the escapes removed (maintained as the bytes arrive). Full-screen programs are judged on the emulator's screen; line output on the stream; titles on the raw bytes.
- **Every check can fail.** Shell results are checked with arithmetic (`echo TAG-$((6*7))` and waiting for `TAG-42`), so the typed echo is never mistaken for the result. Evidence has to be **fresh**: a marker printed after the action, a row that was not on the screen before it, or a text searched from a mark taken before the keystroke, never text that happened to be on the screen already. A job proves it was resumed by printing its own marker on SIGCONT.
- **Sequences are exact.** A counted run (`seq`, ticks, `LINE-n`) must have every line from the first to the last, once, in order. A gap marker (`fil: skipped ...`) is accepted only in the floods, where catch-up is expected, and even there the order must hold, nothing may repeat, a jump needs a marker, and the tail must be whole. Anywhere else a marker fails the scenario (the clients that finish a scenario with their shell are checked for it at the end).
- **Files are compared whole** (`cmp` against the expected content), never by substring.
- A wait stops at once when the client has exited.
- "asserted" below means the code fails the scenario if it does not hold; "measured" is a timing that is recorded and (where stated) bounded; "noted" is recorded only.
- Like a person, the driver waits a moment after a program leaves before typing again (vim, top and readline after Ctrl-C discard what is typed ahead, over ssh too), and answers the client's yes/no prompts after 500 ms of quiet with the key alone, then types nothing for 400 ms (the prompts ignore keys typed sooner). The one place it does not wait is a resize followed by a command (finding 1), because ssh orders that correctly.

## The scenarios

| Scenario | What a person does | What is asserted (and measured) |
|---|---|---|
| `tools` | looks at the box | the required tools are installed; noted: the rest, the locale and shell |
| `vim_edit` | opens a file in vim, types a line, pastes 100 lines, `:wq` | the alternate screen opens and closes; the file on disk is **exactly** the original two lines plus the pasted block; the shell screen comes back. Measured: a typed character on screen after 10 to 30 ms |
| `less_pager` | `less` a 1000-line file: page, `/` search, `G`, `g`, `q` | each key does its job on screen (rows that were not there before); `q` returns to the shell |
| `top_htop` | `top -d 1`, then htop; quit | the display redraws (the header changes at least 3 times in 6 s); `q` quits |
| `git_tools` | makes a repository with 60 commits; `git status`, `git log \| less`, `git log` | clean and dirty status are right; `G` reaches the last commit; git's own pager shows commits that were not on screen before the space |
| `history_completion` | up arrow, Ctrl-R, Tab, double Tab | the command runs exactly once more; the search prompt is a new row; a unique name completes into the command line; the two candidates appear as a new row |
| `ctrl_c_d` | Ctrl-C a command that said it was running, Ctrl-D into `cat`, Ctrl-C at the prompt | exit status 130 and the process gone; `cat`'s file is exactly the typed text; a line cleared by Ctrl-C was not run. Measured: prompt back after ~10 ms |
| `job_control` | Ctrl-Z, `jobs`, `fg`, Ctrl-C, `bg`, a background job | a job that prints its own marker on SIGCONT shows it after `fg` and after `bg`; `jobs` lists it stopped, then running, then not at all after Ctrl-C; a background job's output arrives |
| `flood` | `seq 1 200000`; `cat` of a 4 MB file | each exact under the gap policy above: complete and in order, or marker(s) with order kept and the tail whole. Measured: rates, how many lines were kept |
| `flood_ctrl_c` | Ctrl-C a `yes` flood, on QUIC and on the pipe | the next command's output is back in under 3 s. Measured: tens of ms |
| `unicode` | prints é, 日本語, 🎉; types them | bytes intact; wide characters take two cells; the text after them lines up |
| `title` | sets the terminal title; the shell's prompt sets one | OSC 0 and 2 reach the user's terminal; a title set by the shell's own PS1 does too (a `42` only the prompt can compute) |
| `resize` | resizes the window; ten times in 300 ms; with vim open | `stty size`, `tput` and vim see the new size; the last of ten wins |
| `resize_order` | resizes and types a command at once, 300 times, on QUIC and on the pipe | the command sees the new size (finding 1, **fixed** by #43) |
| `term_colors` | `echo $TERM`, `tput colors`, `ls --color`, 256-color, true color, bold, underline | TERM is the client's `xterm-256color`; every color and attribute arrives exactly |
| `paste_big` | pastes 287 KB into `cat > file` in one write | the file is identical |
| `binary_garbage` | `cat`s a binary by accident, `reset` | the session survives, the terminal recovers, a fresh client finds the same shell |
| `idle` | does nothing for 90 s | the client writes **nothing at all** to the terminal; the first command afterwards answers in under 2 s |
| `detach_reattach` | `~.`, comes back; again after a background job finished | on reattach lines 1..120 are in the native scrollback exactly once, in order, no gap marker; same shell; the background job's output is there once and the job finished |
| `takeover_running` | a second machine takes the session while a command runs | the second sees the running command; the first is told and exits 1; the second's ticks 1..60 are exact, no marker |
| `session_commands` | `fil ls`, `fil detach`, `fil kill` from another shell | listing; the detached client told and exits 1; the killed client ends with 129 (or 137) |
| `exit_status` | `exit 7`, `exit`, `kill -9 $$` | client exit 7, 0, 137, as ssh gives; the session is gone from `fil ls` |
| `vim_session_life` | detaches inside vim, reattaches, a third machine takes over, keeps editing | vim's screen is back each time with all its text; the file is exactly `start`, ROW-1..ROW-7 and the empty line |
| `terminal_modes` | a program holds mouse reporting, application cursor keys, a hidden cursor and the alternate screen; the client reattaches, another takes over | the new client's terminal gets exactly those modes (and bracketed paste at an idle prompt); in vim after a reattach a mouse click moves the cursor and an application-mode arrow moves it down |
| `terminal_sane_after_detach` | `~.` while such a program runs | the local terminal is back to normal screen, mouse off, cursor keys and cursor back, termios cooked (finding 3, fixed by R3) |
| `escape_handling` | `~/x` at the start of a line, `~~/x`, a bracketed paste containing `\n~.`, an unmarked `\r~.` | a lone `~` goes through; `~~` sends one; the bracketed paste does not detach; the unmarked one detaches with status 0, as in ssh, and the session is intact |
| `client_kill9` | the client is killed with SIGKILL; a fresh one attaches (pipe and QUIC) | no question is asked; same shell; the running command's ticks 1..60 are exact; `fil ls` shows the new client attached. Noted: how long the dead one stayed attached (finding 4) |
| `killed_ssh_midcommand` | the ssh dies during a running command | all 80 ticks arrive, exact, no marker; the client logged its link down; status line gone; screen intact |
| `client_sleep` | the laptop sleeps: the client's process group **and its ssh's** are stopped 12 s (QUIC) and 45 s (pipe), then continued | the client logged its link down (so it was a disconnect); ticks exact; status line gone; screen intact; a command answers in under 2 s three seconds later. Measured: first output after the wake. Noted: what `fil ls` said halfway |
| `disrupt_vim` | vim open; ssh killed; then the whole client sleeps 15 s with three lines typed meanwhile | both disconnects seen by the client; vim's screen intact each time; the file is exactly `start`, ROW-1..ROW-9 and the empty line |

Realnet adds, on a 40 ms round trip with real NAT and `tc netem`:

| Scenario | What a person does | What is asserted |
|---|---|---|
| `f_dropin_vim_nat` | types into vim while the NAT mapping moves three times (a new port, another, a new address) | the file is exactly `start`, ROW-1..ROW-12 and the empty line; still on QUIC; no ssh; no gap marker. The router's conntrack proves each move. Measured: the line typed right after each move |
| `f_dropin_vim_loss` | types into vim while UDP drops 30 %, then dies for 6 s, then drops 60 %, then is clean | the file is exactly `start`, ROW-1..ROW-26 and the empty line; the blackout moved the session to the pipe; it came back to QUIC |
| `f_dropin_vs_ssh` | the same battery (vim edit, less, 200000-line flood, 50000-line `cat`, Ctrl-C in a flood, resize, unicode, color, title, Ctrl-Z/`fg`, exit status) through `ssh -tt` and then through fil | each step works on both (vim's file compared whole; `fg` proven by the job's own marker); the final screens of less, the flood and the unicode line are identical between ssh and fil (trailing blanks aside), or the scenario fails. Noted: timings, how many `cat` lines reached each |

## Results

Run on 2026-11-03 (ET). The client is this Mac's, built from the branch after main at `ca90d0b` (S1a, S2, S3, S4, S5 merged); the Pi binary is the `aarch64` job's build of the same tree, sha256 `0bc40457…` ([run 41600284624](https://github.com/anthonytw/fil/actions/runs/41600284624)). Single runs from one LAN, so times are indicative; the two Pis agree within noise.

**Both Pis, 30 scenarios each: pi1 has 28 passes and 2 expected failures; pi2 has 27 passes, 2 expected failures and 1 failure.** The expected failures were `resize_order` and `terminal_sane_after_detach` (findings 1 and 3; reported as XFAIL, they do not fail the run; both have been fixed since, by #43 and R3, and `XFAIL` is now empty, so this table is the run before the fixes). The failure, `ctrl_c_d` on pi2, happened once in the full run because the driver typed a character before bash had redrawn its prompt after a Ctrl-C (readline drops it, over ssh too); the driver now waits for the prompt, and the scenario passed 15 of 15 times on pi2 afterwards. After the last scenario no daemon or socket was left on either Pi.

### The Pis

| Scenario | pi1 | pi2 |
|---|---|---|
| `tools` | vim, vi, less, top, htop, git, nano present; tmux and screen not; bash, UTF-8, Linux aarch64 | same |
| `vim_edit` | pass; a typed character on screen after 15 ms, a 100-line paste after 16 ms; file exactly as typed | pass; 15 ms, 15 ms |
| `less_pager` | pass; page 15 ms, search 15 ms | pass; 15, 14 ms |
| `top_htop` | pass; top's header changed 6 times in 6 s; htop drew and redrew | pass |
| `git_tools` | pass | pass |
| `history_completion` | pass | pass |
| `ctrl_c_d` | pass | **failed once** (the driver's typing race, above); 15 of 15 afterwards |
| `job_control` | pass; `fg` and `bg` each proven by the job's own marker | pass |
| `flood` | pass; `seq 1 200000` complete; `cat` of 3.9 MB: 2 gap markers, 37552 of 50000 lines kept, order kept, nothing repeated, tail whole (finding 2) | pass; `seq` complete; `cat`: 37830 of 50000 kept |
| `flood_ctrl_c` | pass; next output after Ctrl-C in 20 ms (QUIC) and 7 ms (pipe) | pass; 15 and 28 ms |
| `unicode`, `title`, `term_colors` | pass (the prompt's own title included) | pass |
| `resize` | pass; reached the remote in 15 ms; ten resizes ended at the last; vim saw 24x80 | pass |
| `resize_order` | **XFAIL** before #43: QUIC 6 of 200 saw the old size, pipe 1 of 100; after #43: 0 of 1000 (QUIC) and 0 of 500 (pipe) over 5 runs | **XFAIL** before #43: QUIC 6 of 200, pipe 1 of 100; after #43: 0 of 1000 and 0 of 500 |
| `paste_big`, `binary_garbage` | pass | pass |
| `idle` | pass; nothing written to the terminal in 90 s; round trip 6 ms before, 14 ms after | pass; 7 ms, 7 ms |
| `detach_reattach` | pass; last screen after 198 ms, lines 1..120 exactly once in the scrollback, the job's output once | pass; 172 ms |
| `takeover_running` | pass; second client shows the command after 181 ms; first told, exits 1; ticks 1..60 exact | pass; 159 ms |
| `session_commands`, `exit_status` | pass (kill gives 129; exit 7, 0 and 137) | pass |
| `vim_session_life` | pass; file exact | pass |
| `terminal_modes` | pass; a reattach and a takeover restore alternate screen, mouse (SGR), application cursor keys and hidden cursor; vim mouse click and application-mode arrow work after a reattach | pass |
| `terminal_sane_after_detach` | **XFAIL** (before R3): after `~.` the terminal is still in the alternate screen with mouse reporting, application cursor keys and a hidden cursor | **XFAIL**: same |
| `escape_handling` | pass | pass |
| `client_kill9` | pass; after SIGKILL `fil ls` showed the client detached after 0.2 s (pipe) and 5.0 s (QUIC); a fresh client attached with no question, same shell, ticks 1..60 exact | pass; 0.2 s, 5.0 s |
| `killed_ssh_midcommand` | pass; link down seen; 80 ticks exact; the last arrived 17.1 s later; screen intact | pass |
| `client_sleep` | pass; client and ssh stopped 12 s (QUIC) and 45 s (pipe): link down seen, `fil ls` said detached halfway, first output 11 and 13 ms after the wake, ticks exact, round trip back to 7 to 15 ms | pass |
| `disrupt_vim` | pass; after the ssh died the next lines were on screen after 1.1 s; after a 15 s sleep with three lines typed meanwhile all were there 15.8 s after it stopped; file exact | pass; 1.4 s, 15.8 s |

### Realnet (udp part, 40 ms round trip)

[Run 41596719308](https://github.com/anthonytw/fil/actions/runs/41596719308) on the branch with main at `b376112` (S3, S4) merged; **all three parts passed**.

| Scenario | Result |
|---|---|
| `f_dropin_vim_nat` | pass; three NAT moves while typing 12 lines into vim: the line typed right after each move was on screen in 79 to 82 ms; the file is exactly `start`, ROW-1..ROW-12 and the empty line; no further switch, no ssh, no gap marker |
| `f_dropin_vim_loss` | pass; 26 lines typed over 30 % UDP loss, a 6 s blackout and 60 % loss: file exact; the blackout moved the session to the pipe and it came back to QUIC (asserted) |
| `f_dropin_vs_ssh` | pass; every step works on both (vim's file compared whole, `fg` proven by the job's marker); the final screens of less, the flood and the unicode line are identical |

### ssh against fil (realnet, the same battery)

| Step | `ssh -tt` | fil |
|---|---|---|
| vim: open, type, `:wq`; the whole file compared | exact | exact |
| less: page, search; final screen | the reference | identical |
| `seq 1 200000`: final screen | the reference | identical |
| `seq 1 200000`: time to the last line | 328 ms | 368 ms |
| `cat` of 50000 lines (3.9 MB): lines that reached the terminal | **50000** | **25012, three gap markers** |
| Ctrl-C in a `yes` flood, to the next output | 141 ms | 240 ms |
| resize to 40x120 reaches `stty size` | yes | yes |
| unicode line; color escape; title | identical; yes; yes | identical; yes; yes |
| Ctrl-Z, `fg` (the job's own marker), Ctrl-C, no job left | yes | yes |
| `exit 7`: client status | 7 | 7 |

The Pi gate has no shell, so there is no ssh column for the Pis; the table above, from real OpenSSH in the namespace topology, is the only direct comparison. Everything else is judged against what ssh is known to do (the window change and the data are one ordered stream; every byte reaches the scrollback; the terminal is left as the program had it).

## Findings: where fil is worse than ssh, et or mosh

Each has a reproduction. None was fixed here.

1. **FIXED by #43 (in-band resize record, DESIGN §8.5): after the fix, 0 of 1000 (QUIC) and 0 of 500 (pipe) commands saw the old size on each Pi over 5 runs, and the full suite showed no regressions.** Before: **a command typed right after a resize could see the old window size** (formerly an expected failure, `resize_order`). ssh sends the window change in order with the data, so the command always sees the new size; with fil the keystrokes can overtake it. QUIC: 6 of 200 on each Pi; pipe: 1 of 100. It does not happen over a local pipe with no latency (0 of 300 on a private daemon on this machine), so it needs a real round trip. Reproduce: `just dropin --hosts pi1 --only resize_order`, which resizes the pty and at once types `echo RO$i-$(stty size)`. By hand: attach, resize the window and, within a few milliseconds (a script, `expect`, or a paste that follows the drag), run `stty size`. A person typing cannot do this, but a pasted block or an automated session can, and a program that starts straight after (tmux attach, vim, less) can paint for the old size. Where to look: the client queues the resize (from a signal task) and the keys (from a reader thread) into one channel (`crates/fil-client/src/tty.rs`), and the daemon applies the resize from the control stream and the keys from the terminal stream; either hop can reorder them. A fix is to make the resize travel with the input it precedes.
2. **A big `cat` loses lines from the native scrollback; ssh and et do not.** The catch-up rule (DESIGN.md §6.1: more than 1 MiB or 2 s behind jumps to a snapshot, with a visible `fil: skipped` marker) works as designed, and mosh behaves the same way, but it is a step down from ssh and et for the common "cat a log, then scroll back". It triggers every time for a 3.9 MB `cat` (37552 and 37830 of 50000 lines kept on the Pis; 25012 in realnet against ssh's 50000). The checks hold even there: the order is kept, nothing repeats, every jump has a marker and the tail is whole. A stump of the line cut at the skip sits just before the marker. The consumer here is an in-process `vt100` emulator reading a pty, which is slower than a real terminal, so a fast terminal may trigger it less; that is not measured. The threshold is an open question (DESIGN.md §16.7, "catch-up threshold"); these numbers say 1 MiB is low for a 4 MB file. Reproduce: `just dropin --hosts pi1 --only flood`, or `cat` a 4 MB file and look for the marker.
3. **FIXED by R3: `~.` inside a program left the user's terminal in that program's modes** (was the expected failure `terminal_sane_after_detach`; the scenario now has to pass). Before, the client took its own drawing off, printed its message and left raw mode, and nothing more: after detaching from vim or less the shell was on the alternate screen with mouse clicks printing escape sequences, until `reset`. ssh leaves the same mess; mosh restores the terminal on exit, and now fil does too. Now `leave_screen` (`crates/fil-client/src/engine.rs`), which every way of giving the terminal back goes through (`~.`, detached from elsewhere, taken over, the session ended or gone, a signal, a closed terminal, a fatal error, and giving up or detaching while disconnected), writes `restore_sequence` (`crates/fil-client/src/display.rs`) once the session screen was entered. It is built from the shadow screen, which has seen every byte the terminal has since the last snapshot (every snapshot onto a terminal that has been showing the session is preceded by this same reset, because the snapshot writer turns modes on and never off; before that was added, a take-back after the program had quit elsewhere left mouse reporting on, in the session and at exit), so only what is set is reset: mouse reporting and its encodings (1000, 1002, 1003, 1005, 1006, 1015, 1016), focus events, bracketed paste, application cursor keys and keypad, the kitty keyboard protocol (popped on both screens' stacks whenever the stream used it, since the shadow knows only the current screen's flags), the alternate screen, insert mode, autowrap, origin mode and the scroll region (the cursor is put back where it was, since resetting either homes it), the cursor shape; always the modes the shadow's parser cannot see (X10 mouse `?9`, colour-scheme reports `?2031`, in-band resize reports `?2048`), the cursor (shown), xterm `modifyOtherKeys` (back to its initial value; a parser that drops the `>` reads it as SGR 4, which the SGR reset right after undoes; not checked on a real Linux console), the pen, the character sets and an open hyperlink. It never sends `RIS` (the scrollback stays) and leaves the window title as it is, as ssh does (the old one cannot be read back). The cursor ends on a fresh line below the content, then the message. A panic writes the part that is invisible when the modes are already off (mouse, focus, paste, application keys, cursor, pen) after restoring the termios, but not the alternate screen or the kitty stacks, which need the shadow's state; `kill -9` restores nothing. Tests: `crates/fil-client/tests/terminal_restore.rs` (every exit), `display::tests`, and the scenario. Reproduce the old behavior by hand: `printf '\e[?1049h\e[?1000h'; sleep 99`, then `Enter ~.`.
4. **Fixed on main while this was written** (kept so the exact checks are seen to earn their keep):
   - *Output lost with no marker.* On the tree before #39 and #42 were merged, the exact sequence check failed `flood` 2 times in 41 runs on pi1 (a 4 MB `cat` under QUIC): the client logged `a gap in the snapshot; reconnecting` or `a snapshot longer than announced; reconnecting`, reconnected over the pipe, and the fresh snapshot carried no gap marker, so about 40000 lines vanished silently (the line before the jump was always followed by `LINE-048973`, 1027 lines from the end). Every earlier run of the suite had passed it, because its check accepted a marker or the right last line. On main with them merged it did not recur in 61 runs (the full run and 60 repeats of `flood` on pi1).
   - *A killed client stayed "attached".* On the tree before #38 and #40 were merged `fil ls` still showed a client killed with SIGKILL as attached after 57 s, on the pipe and on QUIC. With them merged: 0.2 s on the pipe, 5 s on QUIC.
5. **A flaky check in the existing ssh part of realnet (the suite, not fil).** `f_attach_detach` failed once in an early realnet run ("the earlier output (the shell pid) is not on the second screen") and passed in the three after it. `ssh.sh` waits for the reattached screen to show `FIL-42` and then checks at once for the `PID-` line below it, but the snapshot can arrive in more than one read. The check should wait for the pid line (`fc_wait b "PID-$pid1"`). Not changed here (not this package's file).

Not a finding: typed-ahead input is discarded when vim, top or readline ends or redraws after a Ctrl-C (the tty and the programs do it; it happens over ssh too, and the driver waits as a person does), and the yes/no prompt after a takeover or a remote `fil detach` exists only in fil (ssh has no takeover).

## Mutation checks

The review of PR #41 found three scenarios that still passed with the thing they check broken. Every important check was mutated on purpose and the scenario run again; each now fails. (Local = a private daemon on this machine; Pi = pi1.)

| Scenario | Mutation | Result |
|---|---|---|
| `history_completion` | the double Tab is never pressed | FAIL, no screen row `multi_aaa  multi_bbb` (local) |
| `job_control` | `bg` is never sent | FAIL, the job never printed its resume marker (local) |
| `job_control` | the second Ctrl-Z is never sent | FAIL, the job was not stopped (local) |
| `job_control` | `fg` is never sent | FAIL, no resume marker (local) |
| `vim_session_life` | `ROW-7` typed twice | FAIL, the file is not exactly `start`, ROW-1..ROW-7 and an empty line (local) |
| `git_tools` | the space to git's pager is never sent | FAIL, no commits that were not on screen before (local) |
| `client_sleep` | nothing is stopped | FAIL, the client never logged its link going down: it was not a disconnect (Pi) |
| `killed_ssh_midcommand` | the ssh is not killed | FAIL, the client never logged its link going down (Pi) |
| realnet `dp_finish_vim` | `ROW-5` typed twice | FAIL, `EXACT-42` never appears (local run of the shell helpers) |
| `check_seq` | ticks 13..40 lost with a gap marker; a repeated line; reordered lines; a jump with no marker; two jumps for one marker; a complete run with a marker | each rejected where it must be, by unit tests (`cargo test -p fil --example dropin`, part of `just ci`) |

## Planned (M2)

From the review's list of scenarios that are not covered yet; none is a gap in what M1 claims, all are worth having before the project goes public.

- Reattach or takeover from a terminal of a different size and a different `TERM` (DESIGN.md section 6.4: warn once). Every client here is 30x100 `xterm-256color`.
- Bracketed paste and mouse input passthrough in bash 5 (the vim mouse and arrow checks are done in `terminal_modes`).
- Control keys: Ctrl-S/Ctrl-Q (IXON), Ctrl-\ (SIGQUIT, status 131), Ctrl-V literal, Alt/Meta (ESC prefix), function keys in vim and less.
- Locale and environment: `LANG` and `LC_*` in the session against the client's (ssh `SendEnv`); typed non-ASCII under a UTF-8 locale. `tools` only reports the charmap.
- tmux inside fil (a static tmux through the gate's `install`) and fil inside a local tmux.
- First-connection ssh prompts through the client before the attach (host key, password, 2FA): realnet without `BatchMode`.
- Agent forwarding (DESIGN.md section 10, opt-in) once it lands; OSC 52 and OSC 8 passthrough; a driver that answers DA/DSR queries, so programs do not run on their time-outs.
- Reading the suite on a real terminal application (the emulator here is `vt100`), and a second real client machine.

## What was verified and what was assumed

Verified, on the Pis and in realnet, as above: every scenario in the tables, with the files and screens read back (not just "no error"); the cleanup (no daemon or socket left on either Pi); the real-network scenarios on real NAT and `tc netem`; that each important check fails when what it checks is broken (the mutation table).

Assumed or not covered:

- **ssh behaviour on the Pis** is not measured there: the gate has no shell. ssh's ordered window change (finding 1) is the OpenSSH protocol; its complete scrollback (finding 2) and its leaving the terminal modes as the program had them (finding 4) are from the OpenSSH client and, for the scrollback, the realnet column. **et and mosh** are not installed anywhere in the lab, so claims about them are from their design (mosh skips output as fil does and restores the terminal on exit; et keeps the whole stream).
- **The terminal is an emulator (`vt100`) behind a pty**, not a real terminal program. Nothing here covers how a particular terminal app renders, scrolls its own scrollback, or handles the mouse, focus events or the clipboard (OSC 52). Terminal queries (device attributes, cursor position) get no answer from the driver, so programs that wait for a reply run on their time-outs.
- **No second real client machine.** The takeover is two clients on this one Mac.
- **One network.** The Pis are on the same LAN as the Mac (about 6 ms round trip). Long-haul and mobile cases (Wi-Fi to cellular, a captive portal) exist only as the realnet simulation.
- **NAT rebind and UDP loss in the middle of vim** run in realnet only: the Pis' firewall and the gate offer no way to break UDP.
- **A sleeping laptop** is a stopped process group on a live network. It makes the link go silent and the client see it down (asserted), but the network and NAT state do not change under it as they do when a real laptop moves; that part is the realnet NAT scenarios.
- **Timing** is single runs; the Ctrl-C and flood numbers move by a factor of two between runs.


