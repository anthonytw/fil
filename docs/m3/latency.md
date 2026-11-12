# Prediction on real networks: the latency cells (M3 package V1)

> **Living reference** for the cells it describes and **the record** of their numbers; where it and `DESIGN.md` or the code differ, they win.

The real-network half of M3's verification (`docs/m3/plan.md` §5.5): key-to-glyph latency through the real `fil` client, with its prediction off, `adaptive` and `always`, against `ssh -tt` as the control, on L1's topology (`docs/m2/latency.md`: network namespaces, NAT, `tc netem`, a real sshd). The cells are `tests/realnet/predict.sh` (built by T3, completed by V1). The drop-in half, against the arm64 remote (GitHub's arm64 runner; the Pis optional), is `docs/m3/dropin.md`.

## Running it

The coordinator's route (one run after V1 merges, one at the M3 sweep close; `docs/m3/plan.md` §5.7):

```bash
gh workflow run linux-check.yml --ref ci/BRANCH -f ref=SHA -f suite=latency
```

`suite=latency` runs two realnet jobs, `latency` (M2's lab) and `predict` (these cells), and nothing else. The `predict` job uploads the artifact `realnet-predict`; its summary has the scenario table and a table of the cells. On a Linux box with root: `just realnet predict` (all five) or `just realnet predict p_keys` (one).

**What to keep from the artifact** (everything is under `target/realnet`):

| File | What |
|---|---|
| `results-predict.tsv` | the scenario table: result, seconds, and every measurement as `key=value` |
| `results-predict-cells.tsv` | one row per cell: transport, mode, delay, loss, sent, shown, lost, p50, p99, max, fast, fast p50, vanished, bytes p50, written, mispredicted |
| `run/p_*-TAG.keyprobe`, `.err` | each `keyprobe` result line |
| `run/p_*-TAG.clientlog` | the fil client's debug log of each fil cell (the path check and the counts read it). A throwaway runner's; logs from the Pis are never uploaded |

## How a cell is measured

`keyprobe keys --screen` types into a raw `cat` (or, in `p_shells` and `f_bash`, at a shell's prompt) at 20 keys/s (10 at a prompt) and times each key by when its glyph first shows in its cell of an emulated screen, so a written guess counts. A key shown within half the round trip (`fast`) can only have been a guess: no echo is back that soon. Then:

- **Exact:** every key's glyph appeared (`lost=0`; a `KNOWN-BUG` otherwise), and the final screen holds the keys and nothing after them, the cursor after the last.
- **The path:** a QUIC cell is on QUIC before its first measured key; a pipe cell never is.
- **The counts:** the client detaches (`~.`), so it logs its `PredictStats` (counts only, at debug). With prediction on: no visible misprediction, and no more `fast` keys than guesses written. With it off (and in the `ssh` cells): nothing written and no `fast` key, which also checks that half the round trip still separates a guess from an echo.
- **Prediction helps** (done-when 3, V1): every adaptive `p_keys` cell at 75 ms each way and more has guesses written, keys shown by one, and their p50 under 5 ms.
- **Quiet on a fast link** (done-when 4): in `p_fast` nothing is written, into a raw `cat` (pipe and QUIC) and at a bash prompt (V1 added the prompt: done-when 4 is about shells).
- **Glitches** (`p_loss`): PC-1 calls an echo more than 250 ms late a glitch. `keyprobe` has no per-key list, so the cell records its slowest key against one round trip plus 250 ms (400 ms): a note, not a bar.

## The cells (§5.5) and what covers them

| Cell | Paths | Network | Measured | The bar |
|---|---|---|---|---|
| `p_keys` | fil pipe and QUIC, `off`, `adaptive`, `always`; `ssh -tt -o ObscureKeystrokeTiming=no` as the control | `delay` 25, 75 and 200 ms each way, ±5 ms | key-to-glyph p50 and p99 over all keys and over the `fast` ones (the written guesses), mispredictions, lost keys | zero visible mispredictions; done-when 3 at 75 and 200 ms |
| `p_fast` | fil pipe and QUIC (`cat`), and `auto` at a bash prompt; `adaptive` | `delay` 2.5 ms each way | guesses written | none (done-when 4) |
| `p_loss` | fil QUIC, `adaptive` | 75 ms each way, 2 % and 5 % loss | as `p_keys`; the slowest key against the glitch bound | zero visible mispredictions |
| `p_roam` | fil QUIC, `adaptive` | 75 ms each way, three NAT moves while typing (a new port, another, a new address) | as `p_keys`; each move inside the typing | zero visible mispredictions across the moves |
| `p_shells` | fil `auto`, `adaptive` | 75 ms each way | bash, zsh (`zsh -f`) and fish (`fish --no-config --private`) when installed; bash is required | zero mispredictions, the final screen exact |

**What V1 changed in `predict.sh`** after reading it against §5.5: the done-when 3 check (`pr_helps`), the bash prompt in `p_fast`, the `max_ms` column and the glitch note in `p_loss`, and `fish --private` in `p_shells` (the file's own note asked for it; without it a fish that keeps history offers the last cell's line back as an autosuggestion). The rest was already there from T3.

## Results

Run: `linux-check` `suite=latency`, run 43234330493 on GitHub, on `main` at `84b277e` (2026-11-12, 09:09 to 09:23 ET), GitHub-hosted `ubuntu-24.04` (x86_64), static x86_64 musl binaries. Both jobs green. Every number below is copied from that run's `realnet-predict` artifact (`results-predict-cells.tsv`; the glitch and roaming notes from `results-predict.tsv`). One run: the cells are single runs of 200 keys each (390 in `p_roam`, 55 at a bash prompt), not medians of repeats.

### `p_keys`: key-to-glyph (ms)

| delay each way | path | predict | p50 | p99 | fast | fast p50 | written | mispredicted | lost |
|---|---|---|---|---|---|---|---|---|---|
| 25 | ssh | - | 50.35 | 58.33 | 0 | - | - | - | 0 |
| 25 | pipe | off | 50.69 | 59.18 | 0 | - | - | - | 0 |
| 25 | pipe | adaptive | 0.09 | 53.90 | 188 | 0.09 | 197 | 0 | 0 |
| 25 | pipe | always | 0.10 | 54.83 | 188 | 0.09 | 197 | 0 | 0 |
| 25 | quic | off | 50.95 | 58.72 | 0 | - | - | - | 0 |
| 25 | quic | adaptive | 0.09 | 53.64 | 190 | 0.09 | 199 | 0 | 0 |
| 25 | quic | always | 0.10 | 54.91 | 184 | 0.10 | 193 | 0 | 0 |
| 75 | ssh | - | 150.11 | 158.49 | 0 | - | - | - | 0 |
| 75 | pipe | off | 151.47 | 158.84 | 0 | - | - | - | 0 |
| 75 | pipe | adaptive | 148.06 | 158.89 | 76 | 0.10 | 85 | 0 | 0 |
| 75 | pipe | always | 148.30 | 158.11 | 76 | 0.10 | 85 | 0 | 0 |
| 75 | quic | off | 151.06 | 158.60 | 0 | - | - | - | 0 |
| 75 | quic | adaptive | 146.55 | 157.80 | 76 | 0.10 | 85 | 0 | 0 |
| 75 | quic | always | 147.07 | 159.33 | 76 | 0.10 | 85 | 0 | 0 |
| 200 | ssh | - | 400.55 | 408.86 | 0 | - | - | - | 0 |
| 200 | pipe | off | 401.27 | 408.34 | 0 | - | - | - | 0 |
| 200 | pipe | adaptive | 398.52 | 408.22 | 76 | 0.07 | 85 | 0 | 0 |
| 200 | pipe | always | 396.74 | 408.16 | 76 | 0.07 | 85 | 0 | 0 |
| 200 | quic | off | 401.39 | 408.98 | 0 | - | - | - | 0 |
| 200 | quic | adaptive | 396.49 | 408.53 | 76 | 0.07 | 85 | 0 | 0 |
| 200 | quic | always | 395.97 | 408.26 | 76 | 0.07 | 85 | 0 | 0 |

### `p_fast`, `p_loss`, `p_roam`, `p_shells`

| cell | p50 | p99 | max | fast | written | mispredicted | lost | note |
|---|---|---|---|---|---|---|---|---|
| `f_pipe_adaptive` | 5.34 | 5.38 | 5.40 | 0 | 0 (must be 0) | 0 | 0 |  |
| `f_quic_adaptive` | 5.33 | 7.23 | 8.07 | 0 | 0 (must be 0) | 0 | 0 |  |
| `f_bash_adaptive` | 5.35 | 5.74 | 5.74 | 0 | 0 (must be 0) | 0 | 0 |  |
| `l_quic_adaptive_2` | 147.92 | 353.18 | 360.66 | 76 | 85 | 0 | 0 | glitch: no (max 360.66 ms against 400 ms) |
| `l_quic_adaptive_5` | 149.23 | 402.94 | 502.96 | 76 | 85 | 0 | 0 | glitch: yes (max 502.96 ms against 400 ms; a note, not a bar) |
| `r_quic_adaptive` | 150.23 | 351.55 | 472.29 | 76 | 85 | 0 | 0 | 390 keys; moves at 2579, 7632 and 12958 ms after the first key |
| `s_bash_adaptive` | 0.07 | 0.11 | 0.11 | 55 | 59 | 0 | 0 |  |
| `s_zsh_adaptive` | - | - | - | - | - | - | - | not run: zsh is not installed on the runner (`shells_missing=zsh,fish`) |
| `s_fish_adaptive` | - | - | - | - | - | - | - | not run: fish is not installed on the runner |

### Done-when 2 to 4 on these cells

| Done-when | Holds? |
|---|---|
| 2: no visible misprediction in any cell | **Holds** on these cells: `mispredicted` is 0 in every fil cell with prediction on, `lost` 0 everywhere, and every scenario passed its exact-screen check. Only bash was measured at a prompt (zsh and fish are not on the runner). |
| 3: adaptive at 150 ms round trip and more, p50 over written guesses under 5 ms (and the p50 over all keys next to it) | **Holds as written, with a large caveat.** Over the keys shown by a written guess (`fast`) the p50 is 0.10 ms at a 150 ms round trip and 0.07 ms at 400 ms, on both transports: far under 5 ms. **But only 76 of the 200 keys were shown by a guess** in each adaptive cell at 75 and 200 ms each way (85 written of about 245 guessed, about 150 tentative guesses dropped), so the p50 over all keys is 146.55 to 148.06 ms at a 150 ms round trip and 396.49 to 398.52 ms at 400 ms: about one round trip, within 2 to 5 ms of `off` and of `ssh` (150.11 and 400.55 ms). In this typing cell, one long line that wraps, prediction does not move the median key. At a 50 ms round trip 184 to 190 of 200 keys were shown by a guess and the all-keys p50 is 0.09 to 0.10 ms. This is the known limit 0.2.0 ships with, fixed in 0.2.1: see "Coverage at long round trips" below. |
| 4: adaptive at a 5 ms round trip writes nothing, at a shell prompt included | **Holds:** `written` is 0 in `f_pipe_adaptive`, `f_quic_adaptive` and `f_bash_adaptive`. |

### Coverage at long round trips: the known limit 0.2.0 ships with

**What the 76 of 200 means.** Each `p_keys`, `p_loss` and `p_roam` cell types **one long line**: 210 keys (10 of warm-up) at 20 keys/s into a raw `cat` on a 100-column screen, with no pause, so the line reaches the screen's last column and wraps twice. In every cell at 75 and 200 ms each way, on the pipe and QUIC, with loss and roaming, the counts are the same: about 245 keys guessed, 85 written, 76 of the 200 timed keys shown by a guess, about 150 guesses dropped in tentative epochs (`tentative_dropped` in the client's `PredictStats`). 76 is about what the first row holds after the warm-up and the first confirmation: the guesses stop at the line's first wrap, and at 20 keys/s, faster than one key per round trip, they never resume for the rest of the line. At 25 ms each way 193 to 199 were written, because 20 keys/s is slower than one key per 50 ms round trip. So the number is not a share of everyday typing: it is what one interruption costs when typing never slows down afterwards.

**The limit (the #91 review, `predict-coverage`; owner decision: 0.2.0 ships with it documented, the fix is planned for 0.2.1).** Once something interrupts the run of guesses, prediction stays off until typing slows below about one key per round trip: about 7 keys/s at a 150 ms round trip and 3 keys/s at 400 ms, which is ordinary typing. What interrupts it: Enter, Tab, Ctrl-W, a Backspace while keys are still in flight, reaching the line's end (the wrap above), and fish's or zsh's autosuggestions. Within a line that starts with nothing in flight, prediction works. It costs coverage, never correctness: nothing wrong is shown (every cell above has zero visible mispredictions); fil just does not guess, and the keys appear with their echo, one round trip later, as over ssh. The README and the user guide say the same.
