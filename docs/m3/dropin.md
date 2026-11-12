# Prediction in the drop-in suite (M3 package V1)

> **Living reference** for the scenarios it describes and **the record** of their results; where it and `DESIGN.md` or the code differ, they win.

The M3 half of the drop-in suite (`docs/m1/dropin.md`, `docs/m2/dropin.md`): this Mac's `fil` client with predictive echo on, driven on a pseudo-terminal, against `fil server --stdio` on an arm64 Linux remote host (GitHub's arm64 runner, automated, since the owner's decision of 2026-11-11; the test Pis optionally), or against a private daemon on this machine (`--local`). The plan's list is `docs/m3/plan.md` §5.6; the real-network half (the latency cells) is `docs/m3/latency.md`.

## Running it

```sh
just dropin --list | grep predict_                                    # the seven prediction scenarios
just dropin --local --only predict_shell,predict_secret               # this machine, a private daemon
just dropin --hosts pi1 --only predict_takeover                       # one Pi (FIL_PI_TARGETS, docs/m1/pi.md)
just dropin --predict always --only vim_edit,flood,fwd_cable_pull     # M1 and M2 scenarios with prediction always on
```

`--predict MODE` (new with V1) gives every client the driver starts `--predict MODE`, unless the scenario sets a mode of its own (the prediction scenarios do). The release client's default is `adaptive`, so a run without the flag already predicts wherever the round trip is long enough (above 30 ms, `docs/m3/plan.md` PC-1); on the LAN that is rarely, which is why §5.6 asks for the whole suite again with `always`. `FIL_DROPIN_SUDO=1` adds the `sudo` case to `predict_secret` (one failed sudo attempt, so it is off by default).

## How a prediction scenario checks

The rules of `docs/m1/dropin.md` hold (fresh evidence, exact comparisons, a cleanup before and after each scenario), and a miss is a `MISS:` note that fails the scenario at its end whatever `--allow-*` says (`docs/m2/dropin.md`). In addition, every prediction scenario:

- **types like a person:** one key at a time, about eight a second (`KEY_GAP`, 120 ms), arrows in the terminal's current cursor-key mode;
- **runs through the cut relay with a delay** (`netcut.rs`, `set_delay`): 75 ms each way (`DELAY`), so a host on the LAN is a 150 ms round trip away and `adaptive` predicts;
- **reads the client's own counts:** the client logs `PredictStats` (counts only, at debug) when it ends; the scenario parses that line (`predict::parse_stats`) and requires no visible misprediction (every reason of `Mispredicted` summed; for fish in `predict_shell`, every reason but the two of its named residual, `contradicted` and `hidden`, which plan §2.5 accepts as part of the faithful bar, decided, owner 2026-11-11, and which are reported when they occur; the decision's exclusion of whole writes cannot be told apart here and is the soak's), and guesses written where prediction should show (`written > 0`) or none where it must not (`written == 0`);
- **checks the reserved alphabet** (§5.2): the secrets are Latin-1 letters (U+00C0 to U+00FF, without `×` and `÷`), which no scenario program prints otherwise, and any one of them in the client's output (its escape sequences taken out by the driver's stripper, as a terminal decodes them) or on its emulated screen fails the scenario. The check runs at the end of every prediction scenario, not only the secret one;
- **looks at quiet points:** after each command, once the output has been still for 1.2 s, no cell of the screen is underlined (above 80 ms a written guess is underlined, PC-1, so an underline left behind is a guess that never settled);
- **checks input that is not predicted by timing:** a paste's glyphs must not show within half the round trip of the paste (`nothing_before_echo`): no echo can be back that soon, so anything shown is a guess;
- **compares screens with a fresh attach:** where §5.6 asks for "exact", the client is detached from elsewhere (`fil detach`, so nothing is typed into the session), a new client attaches with prediction off, and the two screens and cursors must be equal.

The **path** is the pipe. With `--local` the relay has no UDP side (the private daemon's QUIC would go round it): the pipe is `nc` to a listener in the driver that runs the private daemon's relay (`fil-daemon-dev --dir DIR relay`) for each connection, so the delay and the pulled cable work on this machine as they do to a Pi. On the Pis the same scenarios use ssh through the relay; QUIC with prediction is the latency cells' record (`p_keys`, `p_loss`, `p_roam`).

**Mutations** (each check broken by hand; the scenario must fail): typing the secret at the shell prompt, where readline echoes it, fails `predict_secret` with "a reserved character (U+00C0) was written to the terminal"; replacing the bracketed paste by one more key of a guessed line fails `predict_paste` with "1 bytes were shown 3 ms after the input, before any echo could be back"; typing `predict_reconnect`'s offline keys while the link is still up fails it with "keys typed while the link was down were written"; the first draft of `predict_resize`, which detached with `~.` (whose Enter draws one more prompt), failed the screen comparison on that one row.

## The scenarios (§5.6)

| Scenario | What a person does | What is asserted (each a miss when it fails) |
|---|---|---|
| `predict_shell` | in bash, zsh (`zsh -f`) and fish (`fish --no-config --private`), those installed, bash required: an edit (Backspace, five lefts, an insert), Up and Enter, Tab completion of a file name, a long line | each line ran exactly as typed; no underline at the quiet points; per shell: zero visible mispredictions and some written |
| `predict_secret` | a secret into `read -s` after its prompt (each prompt is computed, `PW$((0+1)): `, and must be at the cursor, so the echo of the typed command is never taken for it); one typed ahead into `read -s` while `sleep` runs with echo already off; one into a Python `getpass` after its prompt; `sudo -k; sudo true` with `FIL_DROPIN_SUDO=1` | each program got the secret exactly (compared in the session, never printed); no reserved character written or on the screen; no guess retracted under the password rule (it would have been shown) |
| `predict_alt` | vim and less, each started by a client with prediction off; a client with `--predict always` attaches through the relay and types (insert mode, `:wq`; a search, `G`, `q`) | nothing written by the predicting clients, which did predict (their counts line is there, and `always` does not wait for an estimate, so no guess is the alternate-screen rule, PC-9, not the adaptive switch); vim saved exactly what was typed |
| `predict_paste` | after a typed (and guessed) command, a bracketed paste, then an unmarked one (more than 64 bytes in one write, Enter included), into bash, or zsh where bash has no bracketed paste | nothing shown within half the round trip of either paste; the shell ran both lines exactly |
| `predict_reconnect` | a line typed across a killed pipe (ssh, or `nc` with `--local`), then one across a pulled cable, keys typed while the link is known down | both lines ran exactly; nothing of the keys typed while the cable is out (letters the status line never has) is in the client's whole output for 2 s, until the cable is back; zero visible mispredictions. The pipe half types a key as soon as the client sees the link down, but the redial may already be done by then (on this machine it is), so that half has no sure offline window: the cable half is the offline check |
| `predict_resize` | three resizes (narrower, shorter and wider, back) while typing a line, the first with the link held (the relay's cable out for 0.3 s, inside `dead_after`) so it lands on written, unconfirmed guesses on any machine; then a line at the last size | the lines ran exactly; no gap marker; the narrowing met written guesses (`withdrawn.resize > 0`); the screen and cursor equal to a fresh attach's (W1's repaint) |
| `predict_takeover` | a second client takes over while the first types a line: the first's link is held (its cable out, inside `dead_after`) while it types three more keys, and the second attaches around the relay, so the takeover lands on written, unconfirmed guesses on any machine | a guess was showing at the takeover (`withdrawn.takeover > 0`); the first is asked and exits 1, with no underline left at its question; what reached the session is a prefix of what was typed; the second's screen equal to a fresh attach's; zero visible mispredictions on both |

**What is not asserted from outside** and is left to the full-stack harness (T1, `docs/m3/harness.md`), which sees every flush: "no guess written between the resize and its repaint" (`predict_resize` records the repaints asked for and the `resize` withdrawals) and "the first withdraws before its question" beyond the screen at the question (`predict_takeover` records the `takeover` withdrawals). If `predict_resize`'s comparison differs by a row on a Pi, look at its repaint count first: `docs/m3/harness.md` measured the terminal ending different from a fresh snapshot in about one run in five when narrowing, with no guesses at all (the reflow), which W1's repaint is there to remove; the MISS line says how many repaints were asked for. **A secret typed before its prompt is up** is echoed by the remote's own tty (cooked, echo on) and, while that lasts, guessed by the client too; when the program then turns echo off, the guesses of the keys that arrive after it are written and never echoed until the client hears of the change. That is OQ-1's case 2, accepted, and it is what the first macOS x86_64 run of this scenario showed: its prompt wait matched the prompt's text in the echoed command line, the secret went in while Python was still starting, the first letter was the remote's echo and the next two the client's guesses, withdrawn when `getpass`'s echo-off arrived (and `getpass`'s flush threw the keys away). Reproduced here with a `sleep 1` before `getpass`: the reserved letters were written by the remote echo, and every guess of the client's was confirmed by that echo. Nothing was written in a state the rules forbid. The prompts are now computed, so the wait cannot match the command line. Typing a password ahead while the kernel still echoes is OQ-1's case 2, an accepted residual (`docs/m3/plan.md` §4.4): `predict_secret` turns echo off before it types ahead.

## Results

### This Mac, `--local` (2026-11-11, ET)

macOS 27.2 on Apple silicon; the release client, `fil-daemon-dev` and the driver built from `main` at `8ade428` plus this package (`just dropin`, which checks for test hooks). Pipe through the relay at 75 ms each way. No fish here; the macOS bash (3.2) has no bracketed paste, so `predict_paste` used zsh.

| Scenario | Result | Counts (the client's `PredictStats`) | Notes |
|---|---|---|---|
| `predict_shell` | pass (48 s) | bash: written 88, confirmed 88, mispredicted 0; zsh: written 88, confirmed 88, mispredicted 0 | fish not installed |
| `predict_secret` | pass (17 s, 5 of 5 runs) | written 53, confirmed 53, mispredicted 0 | `read -s` after the prompt and typed ahead, `getpass`; no reserved character; sudo not run |
| `predict_alt` | pass (10 s) | vim: written 0; less: written 0 | `--predict always` |
| `predict_paste` | pass (12 s) | written 20, confirmed 20, mispredicted 0 | zsh; nothing shown within 75 ms of either paste |
| `predict_reconnect` | pass (20 s) | written 37, confirmed 37, mispredicted 0 | the cable's loss seen after 4.3 s (`dead_after` is 5 s); no offline key in the output for 2 s |
| `predict_resize` | pass (14 s, 5 of 5 runs) | written 23, confirmed 20, mispredicted 0, withdrawn 3 (resize) | 2 repaints asked for; the screen equal to a fresh attach's |
| `predict_takeover` | pass (14 s, 5 of 5 runs) | first: written 45, confirmed 42, mispredicted 0, withdrawn 3 (takeover); second: written 0 | the link held about 0.4 s; 6 of the 9 keys reached the session |

**Every M1 and M2 scenario with `--predict always`, `--local`:** 54 run, 42 pass, 11 skipped (the scenarios that need ssh as the transport or the relay to a real host, skipped on `--local` as always), 1 failed: `git_tools`, which fails the same way with `--predict off` and passes with `--predict always` under an English locale. It reads `git status`'s English text, which a non-English locale does not print: a limit of the scenario, not of prediction. No regression from prediction.

### The arm64 remote (GitHub's arm64 runner)

**This replaces the Pis as the 0.2.0 gate (decided, owner 2026-11-11).** `linux-check` `suite=arm64-remote` (and `all`) runs this suite, `--scale full --predict always`, from the runner's own client against the static aarch64 build behind the Pis' gate on the same runner (`docs/m1/pi.md` "On GitHub's arm64 runner"). Results on GitHub: none yet; the first dispatch's go here, one column for the runner, in the table's shape below. **Validated locally** (2026-11-12, `main` at `84b277e` plus this branch, the runner's setup in a throwaway systemd container in the Mac's aarch64 VM; `docs/m1/pi.md`): `predict_secret`, `predict_alt`, `predict_paste`, `predict_reconnect`, `predict_resize` and `predict_takeover` pass, with zero visible mispredictions and no reserved character; `predict_shell` passed for bash and zsh and failed for fish in every run, one `contradicted` misprediction each time: fish's named residual (plan §2.5, decided, owner 2026-11-11), which the scenario now accepts (below). The Mac has no fish, so this was the first real-PTY run that showed it. One run of three also showed one `hidden` for bash. Every M1 and M2 scenario passed with `--predict always` (`fwd_ports_view` on a rerun after the last setup fix).

### The Pis (optional)

> **Optional since 2026-11-11** (the owner dropped the Pis as a 0.2.0 gate). PI-TBD stays until someone runs them: two rows per scenario, one per Pi. The Pi binary must come from the `aarch64` job of the same tree as the client.

Run: PI-TBD (commit, date, Pi binary sha256).

| Scenario | pi1 | pi2 | Counts | Notes |
|---|---|---|---|---|
| `predict_shell` | PI-TBD | PI-TBD | PI-TBD | shells installed: PI-TBD |
| `predict_secret` | PI-TBD | PI-TBD | PI-TBD | sudo: PI-TBD |
| `predict_alt` | PI-TBD | PI-TBD | PI-TBD | |
| `predict_paste` | PI-TBD | PI-TBD | PI-TBD | shell: PI-TBD |
| `predict_reconnect` | PI-TBD | PI-TBD | PI-TBD | |
| `predict_resize` | PI-TBD | PI-TBD | PI-TBD | |
| `predict_takeover` | PI-TBD | PI-TBD | PI-TBD | |

**Every M1 and M2 scenario with `--predict always`, the Pis:** PI-TBD (run, pass, skipped, failed per Pi; any failure also run without the flag, to tell a prediction regression from a scenario that fails either way).

### Done-when on the drop-in suite

| Done-when | This Mac | The arm64 remote (the Pis optional) |
|---|---|---|
| 1: no secret written (the reserved-alphabet check) | holds | pending the first `suite=arm64-remote` run |
| 2: no visible misprediction in the prediction scenarios | holds | pending the first `suite=arm64-remote` run |
| 5: every M1 and M2 scenario passes with prediction on | holds for `always` (`git_tools` aside: the locale, above); `adaptive` with injected latency is covered only by the seven prediction scenarios here, not by an M1/M2 re-run through the relay | pending the first `suite=arm64-remote` run |
