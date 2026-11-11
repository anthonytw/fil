# Prediction on real networks: the latency cells (M3 package V1)

> **Living reference** for the cells it describes and **the record** of their numbers; where it and `DESIGN.md` or the code differ, they win.

The real-network half of M3's verification (`docs/m3/plan.md` §5.5): key-to-glyph latency through the real `fil` client, with its prediction off, `adaptive` and `always`, against `ssh -tt` as the control, on L1's topology (`docs/m2/latency.md`: network namespaces, NAT, `tc netem`, a real sshd). The cells are `tests/realnet/predict.sh` (built by T3, completed by V1). The drop-in half, on the Pis, is `docs/m3/dropin.md`.

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

> **To be filled from the coordinator's `linux-check -f suite=latency` run** (the `realnet-predict` artifact's `results-predict-cells.tsv`). Every `TBD` below is a number from that run; none has been measured yet.

Run: `TBD` (commit, date, runner).

### `p_keys`: key-to-glyph (ms)

| delay each way | path | predict | p50 | p99 | fast | fast p50 | written | mispredicted | lost |
|---|---|---|---|---|---|---|---|---|---|
| 25 | ssh | - | TBD | TBD | TBD | TBD | - | - | TBD |
| 25 | pipe | off | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 25 | pipe | adaptive | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 25 | pipe | always | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 25 | quic | off | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 25 | quic | adaptive | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 25 | quic | always | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 75 | ssh | - | TBD | TBD | TBD | TBD | - | - | TBD |
| 75 | pipe | off | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 75 | pipe | adaptive | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 75 | pipe | always | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 75 | quic | off | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 75 | quic | adaptive | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 75 | quic | always | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 200 | ssh | - | TBD | TBD | TBD | TBD | - | - | TBD |
| 200 | pipe | off | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 200 | pipe | adaptive | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 200 | pipe | always | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 200 | quic | off | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 200 | quic | adaptive | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| 200 | quic | always | TBD | TBD | TBD | TBD | TBD | TBD | TBD |

### `p_fast`, `p_loss`, `p_roam`, `p_shells`

| cell | p50 | p99 | max | fast | written | mispredicted | lost | note |
|---|---|---|---|---|---|---|---|---|
| `f_pipe_adaptive` | TBD | TBD | TBD | TBD | TBD (must be 0) | TBD | TBD | |
| `f_quic_adaptive` | TBD | TBD | TBD | TBD | TBD (must be 0) | TBD | TBD | |
| `f_bash_adaptive` | TBD | TBD | TBD | TBD | TBD (must be 0) | TBD | TBD | |
| `l_quic_adaptive_2` | TBD | TBD | TBD | TBD | TBD | TBD | TBD | glitch: TBD |
| `l_quic_adaptive_5` | TBD | TBD | TBD | TBD | TBD | TBD | TBD | glitch: TBD |
| `r_quic_adaptive` | TBD | TBD | TBD | TBD | TBD | TBD | TBD | moves at TBD ms |
| `s_bash_adaptive` | TBD | TBD | TBD | TBD | TBD | TBD | TBD | |
| `s_zsh_adaptive` | TBD | TBD | TBD | TBD | TBD | TBD | TBD | if installed |
| `s_fish_adaptive` | TBD | TBD | TBD | TBD | TBD | TBD | TBD | if installed |

### Done-when 2 to 4 on these cells

| Done-when | Holds? |
|---|---|
| 2: no visible misprediction in any cell | TBD |
| 3: adaptive at 150 ms round trip and more, p50 over written guesses under 5 ms (and the p50 over all keys next to it) | TBD |
| 4: adaptive at a 5 ms round trip writes nothing, at a shell prompt included | TBD |
