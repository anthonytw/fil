# The drop-in suite (M1 package P8)

The owner's rule (2026-11-02): no human testing in the pipeline. "If you're able to use fil as more or less a drop-in replacement for ssh, et, or mosh, then you're on the right track. Automate whatever you can." This suite is that checkpoint. An agent drives the real `fil` client on a pseudo-terminal and does what people do with ssh, et and mosh: edits files in vim, pages with less, watches top, uses git, recalls history, completes names, stops jobs, floods the screen, resizes the window, detaches and comes back, takes a session over from a second machine, loses its ssh, and puts the laptop to sleep. Each scenario asserts that fil behaves at least as well as ssh would. Where it does not, that is a finding (below), not a tolerance.

It is on demand and never part of `just ci`.

## Where it runs

| Where | What | How |
|---|---|---|
| The lab Raspberry Pis (aarch64, Debian 13) | 26 scenarios, this Mac's client against `fil server --stdio` on each Pi through the `fil-test` gate | `crates/fil/examples/dropin.rs`, `just dropin` |
| The real-network harness (`udp` part, a Linux runner) | vim while the NAT mapping moves and while UDP is lossy and dead; the same everyday battery through `ssh -tt` and through fil, side by side | `tests/realnet/udp.sh` (`f_dropin_*`), helpers in `tests/realnet/dropin.sh` |
| This machine (`--local`) | a development aid: the same scenarios against a local `fil server --stdio`. It is not a result | `just dropin --local` |

Every tool runs inside the fil session's shell, which is what a user does. The gate (`fil server --stdio`, `install`, `run`, `status`, `logs`, `reset`) offers no shell, so **plain `ssh -t` cannot be run on the Pis**. The like-for-like comparison with ssh therefore runs in realnet, where the "server" is a real sshd with a real login shell.

## How to rerun

```sh
export FIL_PI_TARGETS="pi1=fil-test@HOST1,pi2=fil-test@HOST2"   # required; the keys are the only hosts it may touch
just dropin                                   # both Pis, every scenario (about 6 minutes per Pi)
just dropin --hosts pi1 --only vim_edit,flood # narrow
just dropin --list                            # the scenario names
just dropin --local                           # this machine, no targets needed
```

The Pi binary must be installed first (`docs/m1/pi.md`), and it must be built from the same tree as the client: the wire format moves between milestones, and a client and daemon from different trees stop at "protocol error". `FIL_PI_TARGETS`, the labels, the cleanup (`fil kill` for this driver's sessions, which are all named `dp-*`, then the gate's `reset`) and the "no daemon left" check are those of `pi_e2e` (`docs/m1/pi.md`). A tool the host lacks (htop, vim...) is skipped with a note, never failed. Do not run two copies at once: `reset` kills everything the account runs.

Realnet: `just realnet udp f_dropin_vim_nat f_dropin_vim_loss f_dropin_vs_ssh` on a Linux box, or the `linux-check` workflow with `-f suite=realnet` (it runs everything; the three scenarios take about one minute each).

## How a scenario checks

- The client runs on a pty (`portable-pty`). What it prints goes through two readers: a `vt100` emulator (what a terminal would show, including the alternate screen, wide cells, colors and attributes) and the raw bytes (escape sequences such as the title). Full-screen programs are judged on the emulator's screen; line output on the stream.
- Shell results are checked with arithmetic (`echo TAG-$((6*7))` and waiting for `TAG-42`), so the typed echo is never mistaken for the result. Files are checked on the remote disk (`cmp`, `grep`, `wc`).
- Long output is judged as the design says: **complete and in order, or the gap marker (`fil: skipped ...`) and the right ending**. Anything else is lost or reordered output and fails.
- Like a person, the driver waits a moment after a program leaves before typing again (vim, top and readline after Ctrl-C discard what is typed ahead, over ssh too). Typing right after a resize is the one place it does not wait, because ssh orders that correctly (finding 1).

## The scenarios

| Scenario | What a person does | What is asserted |
|---|---|---|
| `tools` | looks at the box | which of vim, less, top, htop, git, tmux, screen are installed; locale, shell |
| `vim_edit` | opens a file in vim, types a line, pastes 100 lines, `:wq` | the alternate screen opens and closes; typing shows within 15 to 30 ms; the file on disk is exactly what was typed; the shell screen comes back |
| `less_pager` | `less` a 1000-line file: page, `/` search, `G`, `g`, `q` | each key does its job on screen; `q` returns to the shell |
| `top_htop` | `top -d 1`, then htop; quit | the display redraws (the header changes at least 3 times in 6 s); `q` quits |
| `git_tools` | makes a repository with 60 commits; `git status`, `git log \| less`, `git log` | clean and dirty status are right; both pagers page and quit |
| `history_completion` | up arrow, Ctrl-R, Tab, double Tab | the command comes back and runs; the search prompt opens; a unique name completes; two candidates list |
| `ctrl_c_d` | Ctrl-C a `sleep`, Ctrl-D into `cat`, Ctrl-C at the prompt | exit status 130 and the prompt back in a few ms; `cat` gets its EOF |
| `job_control` | Ctrl-Z, `jobs`, `fg`, `bg`, a background job | each works; the background job's output arrives |
| `flood` | `seq 1 200000`; `cat` of a 4 MB file | `seq` complete and in order; `cat` complete or with the gap marker and the last line (finding 2) |
| `flood_ctrl_c` | Ctrl-C a `yes` flood, on QUIC and on the pipe | the prompt is back within 20 s (measured: tens of ms) |
| `unicode` | prints é, 日本語, 🎉; types them | bytes intact; wide characters take two cells; the text after them lines up |
| `title` | sets the terminal title | OSC 0 and 2 reach the user's terminal; so does the shell's own prompt title |
| `resize` | resizes the window; resizes ten times in 300 ms; resizes with vim open | `stty size`, `tput` and vim see the new size; the last of ten wins |
| `resize_order` | resizes and types a command at once, 30 times, on QUIC and on the pipe | the command sees the new size (**fails: finding 1**) |
| `term_colors` | `echo $TERM`, `tput colors`, `ls --color`, 256-color, true color, bold, underline | TERM is the client's `xterm-256color`; every color and attribute arrives exactly |
| `detach_reattach` | `~.`, comes back; again after a background job finished | the last screen and all 120 earlier lines are in the native scrollback; the same shell; the job's output was waiting |
| `takeover_running` | a second machine takes the session while a command runs | the second sees the running command; the first is told and exits 1; the second's output 1..60 is complete and in order |
| `session_commands` | `fil ls`, `fil detach`, `fil kill` from another shell | listing, the detached client told, the killed client ended |
| `exit_status` | `exit 7`, `exit`, `kill -9 $$` | client exit 7, 0, 137, as ssh gives; the session is gone from `fil ls` |
| `killed_ssh_midcommand` | the ssh dies during a running command | all 80 ticks arrive in order; the status line goes away and leaves the screen undamaged |
| `client_sleep` | the laptop sleeps (SIGSTOP the client) 12 s on QUIC, 45 s on the pipe, then wakes | output resumes within 15 ms; no tick lost; status line gone; screen undamaged; round trip back to normal |
| `vim_session_life` | detaches inside vim, reattaches, a third machine takes over, keeps editing | vim's screen is back each time with all its text; the file has every line in order |
| `disrupt_vim` | vim open; ssh killed; then the client sleeps 15 s with three lines typed meanwhile | every line reaches the file in order; vim's screen is intact each time |
| `paste_big` | pastes 287 KB into `cat > file` in one write | the file is identical |
| `binary_garbage` | `cat`s a binary by accident, `reset` | the session survives, the terminal recovers, a fresh client finds the same shell |
| `idle` | does nothing for 90 s | the status line never appears; the round trip is unchanged |

Realnet adds, on a 40 ms round trip with real NAT and `tc netem`:

| Scenario | What a person does | What is asserted |
|---|---|---|
| `f_dropin_vim_nat` | types into vim while the NAT mapping moves three times (a new port, another, a new address) | 12 lines in the file once each in order; still on QUIC; no ssh; no gap marker. The router's conntrack proves each move |
| `f_dropin_vim_loss` | types into vim while UDP drops 30 %, then dies for 6 s, then drops 60 %, then is clean | 26 lines in the file once each in order; the session falls back to the pipe and returns to QUIC |
| `f_dropin_vs_ssh` | the same battery (vim edit, less, 200000-line flood, 50000-line `cat`, Ctrl-C in a flood, resize, unicode, color, title, Ctrl-Z/`fg`, exit status) through `ssh -tt` and then through fil | each step works on both; the final screens of less, the flood and the unicode line are byte-identical between ssh and fil |

## Results

Run on 2026-11-03 (ET). The client is this Mac's, built from the branch at the commit that was main (`cef747c`, the wire changes of #35) plus this example; the Pi binary is the `aarch64` job's build of the same tree, sha256 `7b1f5e6c…` ([run 41587815283](https://github.com/anthonytw/fil/actions/runs/41587815283)). An earlier pass of the same scenarios on the previous main (binary `586d2667…`, [run 41580744529](https://github.com/anthonytw/fil/actions/runs/41580744529)) agreed. Single runs from one LAN, so times are indicative; the two Pis agree within noise. Both Pis: **25 of 26 scenarios pass; `resize_order` fails (finding 1)**. After the last scenario a daemon or socket left behind is a failure, and none was. After the later merges of #36 (repo hygiene) and the 400 ms pause before answering a prompt (S2's PR #39, in review), `takeover_running`, `session_commands` and `vim_session_life` were rerun on both Pis and passed.

### The Pis

| Scenario | pi1 | pi2 |
|---|---|---|
| `tools` | everything present except tmux and screen (htop, git 2.47, less 668); bash, UTF-8 | same |
| `vim_edit` | pass; vim drawn 28 ms after Enter, a typed character on screen after 15 ms, a 100-line paste after 16 ms; file identical | pass; 36, 15, 15 ms |
| `less_pager` | pass; page 15 ms, search 14 ms | pass; 15, 13 ms |
| `top_htop` | pass; top's header changed 6 times in 6 s; htop drew and redrew | pass; same |
| `git_tools` | pass | pass |
| `history_completion` | pass; up arrow 15 ms, Tab 10 ms | pass; 11, 11 ms |
| `ctrl_c_d` | pass; status 130, prompt back after 7 ms | pass; 15 ms |
| `job_control` | pass; Ctrl-Z stopped the job in 15 ms | pass; 13 ms |
| `flood` | pass; `seq 1 200000` complete and in order in 0.1 s; `cat` of 3.9 MB: **gap marker, 25347 of 50000 lines kept** (finding 2) | pass; `seq`: **gap marker, 127877 of 200000 kept** in this run (complete in the two earlier runs); `cat`: gap marker, 34980 of 50000 kept |
| `flood_ctrl_c` | pass; prompt back 49 ms (QUIC) and 31 ms (pipe) after Ctrl-C | pass; 29 and 126 ms |
| `unicode` | pass | pass |
| `title` | pass; OSC 0, OSC 2 and the prompt's own title reach the terminal | pass |
| `resize` | pass; reached the remote in 123 ms; ten resizes ended at the last; vim saw 24x80 | pass; 130 ms |
| `resize_order` | **FAIL**: QUIC 1 of 30 saw the old size (other runs: 1 to 5 of 30); pipe 0 | **FAIL**: QUIC 1 of 30, pipe 1 of 30 |
| `term_colors` | pass | pass |
| `detach_reattach` | pass; last screen 174 ms after the client started, all 120 earlier lines in the scrollback; the background job's output was waiting | pass; 353 ms |
| `takeover_running` | pass; the second client shows the command in 166 ms, the first is told within 181 ms and exits 1; ticks 1..60 complete | pass; 159 ms |
| `session_commands` | pass; `fil kill` ends the client with status 129 | pass |
| `exit_status` | pass; 7, 0, 137 | pass |
| `killed_ssh_midcommand` | pass; 80 ticks in order after the ssh died at tick 12; screen intact | pass |
| `client_sleep` | pass; asleep 12 s on QUIC and 45 s on the pipe: first output 10 and 15 ms after the wake, no tick lost, status line gone, screen intact, round trip back to 5 to 12 ms | pass |
| `vim_session_life` | pass; reattach inside vim 167 ms | pass; 166 ms |
| `disrupt_vim` | pass; ssh killed: next lines on screen after 1.35 s; asleep 15 s with three lines typed meanwhile: all there 15.7 s after it stopped; file in order | pass |
| `paste_big` | pass; 287 KB identical in 0.1 s | pass |
| `binary_garbage` | pass | pass |
| `idle` | pass; no status line in 90 s | pass |

### Realnet (udp part, 40 ms round trip)

[Run 41586959619](https://github.com/anthonytw/fil/actions/runs/41586959619), on the branch with main at `cef747c` merged; all three parts passed. A first run, [41583772925](https://github.com/anthonytw/fil/actions/runs/41583772925), passed the new scenarios too; its ssh part failed once in `f_attach_detach` (finding 4).

| Scenario | Result |
|---|---|
| `f_dropin_vim_nat` | pass; three NAT moves (a new port, another, a new address) while typing 12 lines into vim: the line typed right after each move was on screen in 75 to 77 ms; the file has all 12 in order; no further switch, no ssh, no gap marker |
| `f_dropin_vim_loss` | pass; 26 lines typed over 30 % UDP loss, a 6 s blackout and 60 % loss: all in the file in order; the client went to the pipe once for the blackout and came back to QUIC once after it |
| `f_dropin_vs_ssh` | pass; every step works on both; the final screens of less, the 200000-line flood and the unicode line are identical |

### ssh against fil (realnet, the same battery)

| Step | `ssh -tt` | fil |
|---|---|---|
| vim: open, type, `:wq`; file correct | yes | yes |
| less: page, search; final screen | the reference | identical |
| `seq 1 200000`: final screen | the reference | identical |
| `seq 1 200000`: time to the last line | 308 and 320 ms | 370 and 385 ms |
| `cat` of 50000 lines (3.9 MB): lines that reached the terminal | **50000** | **25350, two gap markers** |
| Ctrl-C in a `yes` flood, to the next output | 153 and 200 ms | 276 and 173 ms |
| resize to 40x120 reaches `stty size` | yes | yes |
| unicode line; color escape; title | identical; yes; yes | identical; yes; yes |
| Ctrl-Z, `fg`, Ctrl-C: status 130 | yes | yes |
| `exit 7`: client status | 7 | 7 |

The Pi gate has no shell, so there is no ssh column for the Pis; the table above, from real OpenSSH in the namespace topology, is the only direct comparison. Everything else is judged against what ssh is known to do (the window change and the data are one ordered stream; every byte reaches the scrollback).

## Findings: where fil is worse than ssh, et or mosh

Each has a reproduction. None was fixed here.

1. **A command typed right after a resize can see the old window size.** ssh sends the window change in order with the data, so the command always sees the new size; with fil the keystrokes can overtake it. It happens on QUIC and on the pipe, on both Pis, in every run: 1 to 5 of 30. It does not happen over a local stdio pipe with no latency (0 of 30, four runs), so it needs a real round trip. Reproduce: `just dropin --hosts pi1 --only resize_order`, which resizes the pty and at once types `echo RO$i-$(stty size)`. By hand: attach, resize the window and, within a few milliseconds (a script, `expect`, or a paste that follows the drag), run `stty size`. A person typing cannot do this, but a pasted block or an automated session can, and a program that starts straight after (tmux attach, vim, less) can paint for the old size. Where to look: the client queues the resize (from a signal task) and the keys (from a reader thread) into one channel (`crates/fil-client/src/tty.rs`), and the daemon applies the resize from the control stream and the keys from the terminal stream; either hop can reorder them. A fix is to make the resize travel with the input it precedes.
2. **A big `cat` loses lines from the native scrollback; ssh and et do not.** The catch-up rule (DESIGN.md §6.1: more than 1 MiB or 2 s behind jumps to a snapshot, with a visible `fil: skipped` marker) works as designed, and mosh behaves the same way, but it is a step down from ssh and et for the common "cat a log, then scroll back". It triggered in every run for a 3.9 MB `cat` (25347 to 45608 of 50000 lines kept on the Pis; 25350 in realnet against ssh's 50000), and once for a 1.3 MB `seq 1 200000` (127877 of 200000 on pi2; complete on pi1 and in the other runs). The consumer here is an in-process `vt100` emulator reading a pty, which is slower than a real terminal, so a fast terminal may trigger it less; that is not measured. The last line and the screen are always right. Reproduce: `just dropin --hosts pi1 --only flood`; or `cat` a 4 MB file and look for the marker. The threshold is an open question (DESIGN.md §16.7, "catch-up threshold"); these numbers say 1 MiB is low for a 4 MB file.
3. **Nothing else was found worse than ssh in the 26 scenarios.** No lost or duplicated keystroke, no screen damage after a disruption, no lost output across a killed ssh or a 45 s client sleep. Differences that are not findings: typed-ahead input is discarded when vim, top or readline ends (the tty and the programs do it; it happens over ssh too), and the yes/no prompt after a takeover or a remote `fil detach` exists only in fil (ssh has no takeover).
4. **A flaky check in the existing ssh part of realnet (the suite, not fil).** `f_attach_detach` failed once in the first realnet run ("the earlier output (the shell pid) is not on the second screen") and passed in the second. `ssh.sh` waits for the reattached screen to show `FIL-42` and then checks at once for the `PID-` line below it, but the snapshot can arrive in more than one read, so the second line may still be in flight. The screen saved at the failure has both lines. The check should wait for the pid line (`fc_wait b "PID-$pid1"`) instead of testing it once. Not changed here (not this package's file).

## What was verified and what was assumed

Verified, on the Pis and in realnet, as above: every scenario in the tables, with the files and screens read back (not just "no error"); the cleanup (no daemon or socket left on either Pi); the real-network scenarios on real NAT and `tc netem`.

Assumed or not covered:

- **ssh behaviour on the Pis** is not measured there: the gate has no shell. ssh's ordered window change (finding 1) is the OpenSSH protocol; its complete scrollback (finding 2) is the realnet column. **et and mosh** are not installed anywhere in the lab, so claims about them are from their design (mosh skips output as fil does; et keeps the whole stream).
- **The terminal is an emulator (`vt100`) behind a pty**, not a real terminal program. Nothing here covers how a particular terminal app renders, scrolls its own scrollback, or handles the mouse, focus events or the clipboard (OSC 52). Terminal queries (device attributes, cursor position) get no answer from the driver, so programs that wait for a reply run on their time-outs.
- **No second real client machine.** The takeover is two clients on this one Mac.
- **One network.** The Pis are on the same LAN as the Mac (about 6 ms round trip). Long-haul and mobile cases (Wi-Fi to cellular, a captive portal) exist only as the realnet simulation.
- **NAT rebind and UDP loss in the middle of vim** run in realnet only: the Pis' firewall and the gate offer no way to break UDP.
- **Timing** is single runs; the Ctrl-C and flood numbers move by a factor of two between runs.

