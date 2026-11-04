# Real terminals (M3, package T2)

> **Status: built and run on 2026-11-04** (`tests/terminals/`, `linux-check -f suite=terminals`). The plan is `docs/m3/plan.md` §3 "T2" and §5.3. `DESIGN.md` stays the source of truth; this file is the evidence behind the changes the findings ask for.

Until now the prediction core, the snapshot writer and the status line were checked against `alacritty_terminal` and `vt100` only. This suite replays what fil writes into **real terminals**, reads their screens back by the terminals' own commands, and compares each screen with the shadow model (`alacritty_terminal` through `fil-term`, the emulator the daemon and the client's shadow screen run). Every difference is reported as a **disagreement between the model and a terminal**. On the screen, that is corruption of the user's view, not a misprediction.

## What is replayed

`tests/terminals/exporter` (its own cargo workspace; not part of `just ci`) writes one JSON trace per scenario. A trace is a list of steps: bytes to write, resizes, and checkpoints with the expected screen, cursor, cell attributes and scrollback taken from the model fed the same bytes. 89 traces, about 520 checkpoints:

| Group | Traces | What the terminal is fed |
|---|---|---|
| `ov-*` | 20 hazard traces of the spike (`crates/fil-predict/tests/hazards.rs`, one per row of the hazard table), each with synchronized output on and off; `ov-typing`, a typing session with the default predictor at 200 ms | The real `Overlay`'s output: the erase, the server bytes, the paint, and the `?2026` markers, with a checkpoint after each flush and, where the erase is at rest, right after the erase |
| `st-*` | 6 status-line scenarios (`crates/fil-predict/tests/status.rs`), with and without synchronized output | The same, for the pinned status (inverse video) |
| `sn-*` | 9 snapshots: plain, history, wrapped lines, styles, wide characters, alternate screen, scroll region with origin mode and tab stops and DEC graphics, pending wrap, autowrap off | `fil_term::snapshot::snapshot` of a state, then the probe |
| `as-*` | 10 micro-tests of the assumptions `docs/spikes/prediction.md` lists as unverified | Hand-written bytes |
| `wd-widths` | 30 strings | Each printed at the start of a row; the cursor position report says how wide the terminal thinks it is |
| `rf-*` | 7 reflow cases | Wrapped lines, history, a prompt with typed text, wide characters at the edge, a snapshot; narrowed, widened, returned |
| `cl-*` | 6 clears | A screen with scrollback, then `CSI H CSI J` (the snapshot writer's clear), `CSI H CSI 2 J`, or row-by-row `CSI 2 K`; and the same over a short session |
| `canary` | 1 | A deliberately wrong expectation. The run fails if the terminal agrees with it: a blind read-back must not pass |

Two details keep the traces honest:

- **True-order resizes.** The terminal resizes (and reflows) first; only then does the client's overlay learn of it and erase. The harness's own driver erases first, which no terminal does (`docs/m3/plan.md` §1, the M0 finding).
- **The probe.** After most traces the exporter writes follow-up bytes that make hidden state visible: the character set, the pen, the saved cursor, origin mode, autowrap and pending wrap, and the scroll region (`fil_predict::oracle::PROBE`, with the last column spelled out because WezTerm 20240203 puts the cursor one column past the edge for `CSI 999;999 H`).

A checkpoint is skipped while the terminal's parser is inside an escape sequence or the program's own `?2026` batch is open (the read-back's cursor position report would land in the middle of the sequence).

The exporter's self-check (`cargo test` in `tests/terminals/exporter`, run first by `run.sh`) replays every trace, with the synchronized-output markers, into a fresh `alacritty_terminal`, and every checkpoint must hold (they do; the one informational checkpoint, the inside of a synchronized-output bracket, is not replayed).

## How each terminal is read back

`tests/terminals/player.py` runs inside the terminal as its program, writes the trace to its own pty, and at each checkpoint sends a cursor position report as a **barrier** (the terminal answers only after it has parsed everything before it; the answer is the cursor to compare), then reads the screen back.

| Terminal | Version | Started as | Screen | Scrollback | Resize |
|---|---|---|---|---|---|
| xterm | 390 (Ubuntu 24.04) | `xterm -geometry CxR` | `CSI i` with `printerCommand` and `printAttributes: 2`, appended to a file | `print-immediate` bound to F5, sent with `xdotool`: the saved lines and the screen in one file | `CSI 8;R;C t` (`allowWindowOps`) |
| kitty | 0.32.2 (Ubuntu 24.04), Mesa llvmpipe | `kitty --config NONE -o allow_remote_control=socket-only -o listen_on=unix:...` | `kitty @ get-text --ansi --add-wrap-markers` (without the markers soft-wrapped lines are joined) | `--extent all` less the screen | `kitty @ resize-os-window --self --unit cells` |
| WezTerm | 20240203-110809-5046fc22, the upstream `.deb` pinned by SHA-256 | `wezterm start --always-new-process` with a generated config | `wezterm cli get-text --escapes --start-line 0 --end-line R-1` (without a range the text starts above the viewport after a scroll) | `--start-line -N`, with N from the pane's dimensions: a `user-var-changed` handler in the generated config writes `scrollback_rows` and `physical_top` to a file when the player sets a user variable (OSC 1337) | `xdotool windowsize` in whole cells (zero padding) |
| tmux | 3.4 (Ubuntu 24.04) | `tmux -L SOCKET new-session -d -x C -y R` | `capture-pane -p -e` | `capture-pane -S -N -E -1`, N from `#{history_size}` | `resize-window -x C -y R` |

macOS terminals are not read back (`docs/m3/plan.md` §3 T2): that needs an Automation consent, which is the owner's call.

### Quirks of the read-back itself

These are not findings about fil; each is handled in `player.py`, and each is a reason a checkpoint compares less than it seems to.

- A cursor position report with a **wrap pending** is `(row, cols+1)` in tmux and `(row+1, 1)` in kitty; accepted when the model has a pending wrap.
- In **origin mode** tmux reports the cursor absolute (xterm and WezTerm relative to the region, as the standard says); kitty's report is wrong by a constant (it was off by one in a plain case, by two in another) and is not compared there.
- **xterm's print-screen** does not write a cell's attributes across blanks and does not go back to the default after an attributed run, so the attributes of blank cells are not compared on xterm, and neither is a cell that has exactly the flags of the cell before it unless the model expects them. A bug that spread an attribute to the *next* cell would not be seen on xterm.
- **WezTerm's** `get-text --escapes` prints no curly underline, so the curly cells of `sn-styles` and `as-pen` cannot be read back; and `get-text` ends with an extra empty row.
- Inside a synchronized-output bracket **WezTerm holds its replies** until the end marker (no cursor report in two seconds); the check is informational.
- Some of the model's own reports are unreliable after DECSTR (alacritty reports no scroll region while keeping one), which shows as a cursor difference in `as-decstr/origin-mode`.

## Results

`tests/terminals/known.tsv` lists every disagreement the run knows about, with a reason. A disagreement that is **not** listed fails the run (`TERMINALS_STRICT=1` ignores the list); a listed one is reported and does not. Today: **no unlisted disagreement, no read-back error, the canary caught on all four.**

| Terminal | Checkpoints | Agree | Disagree (all listed) |
|---|---|---|---|
| xterm | 521 | 454 | 67 |
| kitty | 521 | 481 | 40 |
| WezTerm | 521 | 468 | 52 (and 1 informational) |
| tmux | 521 | 492 | 29 |

**The overlay's own writes agree on all four terminals**, with synchronized output on and off, at every flush checkpoint and at every erase point where the erase is at rest, for all 20 hazard traces, the typing session, and all 6 status-line scenarios, *except across a resize and where the terminal does not understand the SGR the program itself sent*. That is the answer to "can the erase be wrong on a real terminal": not outside a resize. The disagreements that remain fall into the findings below.

### Verdicts on the assumptions of `docs/spikes/prediction.md`

| Assumption | xterm | kitty | WezTerm | tmux |
|---|---|---|---|---|
| `?2026` is ignored where unsupported; text is shown | agrees (ignored) | agrees; supports the mode (its parser logs a timeout when a bracket has no end) | agrees; supports it, and holds cursor reports back inside a bracket | agrees (ignored) |
| A `CUP` clears a pending wrap | agrees | agrees | agrees | agrees |
| Overwriting the last cell of a soft-wrapped line loses the wrap flag (the guard's premise) | no reflow: unobservable | agrees, the flag is lost | agrees, the flag is lost | **disagrees: tmux keeps the flag**; the guard is only conservative there |
| DECSTR resets the charsets, insert mode, origin mode, the saved cursor | resets all four | **ignores DECSTR** (it logs it as unknown) | resets all four | resets the charsets only |
| `CSI ? 1049 l` with nothing saved | resets the charset | agrees with alacritty (keeps it) | agrees with alacritty (keeps it) | resets the charset |
| An unknown private mode is ignored | agrees | agrees | agrees | agrees |
| Underline next to a color pen | the plain forms agree; **the colon forms and SGR 58 are misread** (finding 6) | agrees | plain forms agree; curly underline cannot be read back | agrees |
| Widths | finding 5 | finding 5 | finding 5 | finding 5 |

## Findings

### 1. Reflow differs from the shadow on kitty and WezTerm; xterm does not reflow (W1's second trigger)

`rf-*` and `ov-resize-*`, the plan's "reflow on narrowing and widening, compared with `alacritty_terminal`". The shadow's rows after a resize are not the terminal's rows on three of the four.

| Case | alacritty (the shadow) | xterm | kitty | WezTerm | tmux |
|---|---|---|---|---|---|
| Narrowing (80 to 40 or 30 columns) | keeps the cursor on its screen row and pushes the rows above it into the scrollback, even when the screen has room | no reflow: long lines are clipped and the text beyond the new width is lost (content check fails) | **keeps the top: nothing is pushed, the rows grow downward** | same as kitty | **same as alacritty** in every case |
| Widening | pulls lines back from the scrollback to fill the screen, rejoins wrapped lines | no reflow; text lost earlier does not come back | rejoins, but **does not pull lines back**: blank rows are left at the bottom | rejoins and pulls back, but ends **one row off** when it returns to the start size | same as alacritty |
| Height only (24 to 12 to 24 rows) | pulls the scrollback back when growing | agrees | **does not pull back** (cursor stays on row 12) | agrees | agrees |
| Cursor after a widening in the middle of a wrapped line | column n | n/a | **one column further** | agrees | agrees |
| A wide character that does not fit at the right margin, then narrowed and widened | moves it to the next row, leaving a spacer | clipped | a space is left before it at 60 columns | **drops it already when it is printed** (before any resize) | the cursor ends on the wrong row after the return |

Consequence: after any resize that reflows, the shadow no longer says what the terminal shows on kitty, WezTerm and xterm, even where nothing was drawn. The client's erase (restore cells from the shadow) then writes the wrong characters at the wrong places (`ov-resize-narrow` on kitty and WezTerm: the row that holds the guess comes back as `...wrtp morehir` instead of `...wrap at thirty more`). That is exactly the plan's §2.4 rule 9 case ("the screen is untrusted after a resize") and the second trigger W1's gate names: **W1 is justified; the lazy repaint is the only repair that does not depend on the terminal**. The repaint must replace the whole screen, because even the row order differs.

tmux alone behaves like the shadow here; do not take it as evidence about the others.

### 2. DECSC/DECRC and DECSTR: the shadow keeps state the terminals drop (known, now measured)

`ov-origin*`, `ov-decstr*`, `ov-restore-without-save*`, `as-decstr`, `as-restore-without-save`, `sn-region-tabs-charset`. All four are the rows of the spike's hazard table (7 and the DECSC note) turning out as described, on real terminals:

- A restore (`ESC 8`) with nothing saved, and a restore after the snapshot, restores **origin mode** on xterm, WezTerm, kitty and tmux; alacritty does not save it. The tracker marks origin mode unknown after a restore, so nothing is painted in that state.
- DECSTR resets the charsets (xterm, WezTerm, tmux), insert and origin mode and the saved cursor (xterm, WezTerm); alacritty keeps them; kitty ignores DECSTR entirely. After DECSTR the shadow can also hold a **saved cursor** the terminal no longer has (`as-decstr/saved-cursor`): a program that does DECSTR and then DECRC lands at its old position in the shadow and at home on xterm and WezTerm, so the cursor the client restores after the next server bytes would be wrong. The tracker does not list the saved cursor as unknown after DECSTR. This is a **new, small guard to add** (by whichever package holds `tracker.rs` then): treat the cursor as unknown after a DECSTR until the next explicit cursor move, or paint nothing. It needs a program that sends DECSTR then DECRC, which is rare.
- kitty keeps the current pen across a restore with nothing saved.

### 3. The snapshot writer's clear pushes the screen into the scrollback on tmux (W1's clear)

`cl-*`. Scrollback lines after the clear, over a full screen with 32 lines of scrollback (the "short" row is a screen holding three lines and no scrollback):

| Clear | alacritty (model) | xterm | kitty | WezTerm | tmux |
|---|---|---|---|---|---|
| `CSI H CSI J` (the snapshot writer's), full screen | 32 | 32 | 32 | 32 | **55** |
| `CSI H CSI J`, short session | 0 | 0 | 0 | 0 | **3** |
| `CSI H CSI 2 J`, full screen | **55** | 32 | 32 | 32 | **55** |
| `CSI H CSI 2 J`, short session | **3** | 0 | 0 | 0 | **3** |
| `CSI 2 K` on each row, then `CSI H` (full / short) | 32 / 0 | 32 / 0 | 32 / 0 | 32 / 0 | 32 / 0 |

- **tmux pushes the visible screen into its history on `CSI H CSI J`**, and the visible lines of a short session too. A snapshot (attach, catch-up, resume) and W1's repaint therefore cost one screen of tmux scrollback each time. For W1 the plan's fallback applies to tmux: "a repaint on such a terminal costs one screen of scrollback per repaint".
- **The row-by-row `CSI 2 K` clear pushes nothing on any of the four, and the model agrees.** That is the clear W1 can use if it wants one that is "safe on all four terminals": cursor to each row, `CSI 2 K`, cursor home. (The pen is reset first, as the snapshot writer already does, so the erase fills with the default background.) It writes 24 more small sequences per snapshot and does nothing about the scrollback tmux already has.
- `CSI 2 J` is not a substitute: tmux pushes on it too, and the model does as well (next finding).

### 4. The shadow pushes a cleared screen into its history; xterm, kitty and WezTerm do not

`cl-ed2`, `ov-clear-screen`. On `CSI 2 J` alacritty copies the screen into its scrollback (55 lines in the table above), and xterm, kitty and WezTerm keep their scrollback as it was (32). The daemon's snapshot history is rendered from the shadow, so after a program clears the screen with `CSI 2 J` the snapshot's history contains the cleared screen and the user's own scrollback does not. The next attach of a client with a clean terminal shows lines it never had. Harmless for the screen; worth knowing for K1's catch-up decision and for any test that compares history with a terminal's. `CSI H CSI J` is the other way round: the model does not push, tmux does.

### 5. Widths: where `unicode-width`, the shadow and the terminals disagree

`wd-widths`. Columns the cursor advances after printing the string at the start of a row. A `*` marks a row where the five columns are not all equal.

| String | `unicode-width` (the overlay's table) | alacritty (shadow) | xterm | kitty | WezTerm | tmux |
|---|---|---|---|---|---|---|
| ASCII, Latin-1, Latin Extended-A, combining acute, Greek, box drawing, middle dot, plus-minus, curly quote, black star, circled 1 (the "ambiguous" ones), check mark, halfwidth katakana | 1 | 1 | 1 | 1 | 1 | 1 |
| CJK, fullwidth, Hangul, emoji with default emoji presentation, a regional-indicator pair, ZWSP in `x​y` | 2 (pair: 2; ZWSP string: 2) | same | same | same | same | same |
| `U+2764 U+FE0F` (heart VS16), smiley VS16, `1 U+FE0F`, keycap | **2** | **1** | 1 | **2** | 1 | **2** |
| thumbs up with skin tone | 2 | **4** | **4** | 2 | 2 | 2 |
| ZWJ family (three emoji joined) | 2 | **6** | **6** | **6** | 2 | 2 |
| `x U+00AD y` (soft hyphen) | 2 | 2 | **3** | 2 | 2 | **3** |
| a lone regional indicator | 1 | 1 | 1 | **2** | 1 | **2** |

- **Ambiguous-width characters are not a problem on these four**: every one measures 1 everywhere.
- **VS16 sequences, skin tones, ZWJ sequences, the soft hyphen and a lone regional indicator are disputed.** The overlay's `unicode-width` table is *not* the shadow's table (it says 2 for VS16, 2 for the skin tone and ZWJ strings where alacritty says 1, 4 and 6), and the terminals split three ways. A cell restored from the shadow after the client has painted next to such a sequence can land one column off. `crates/fil-predict` paints only printable ASCII and Latin letters, which are 1 everywhere, so the core is safe by construction; the **P1 rule** "no guess next to a cell T2's table marks as disputed" needs the list: any cell whose text contains `U+FE0F`, `U+200D`, a skin-tone modifier (`U+1F3FB` to `U+1F3FF`), `U+00AD` or a regional indicator.
- The snapshot writer writes such cells as the shadow's text; the only visible effect seen is an extra blank after a VS16 heart on tmux and a missing one on WezTerm (`sn-wide`).

### 6. xterm 390 misreads the colon forms and SGR 58 that the snapshot writer emits

`sn-styles`, `as-pen`, `ov-pen`. `fil_term::snapshot::sgr` writes a curly underline as `4:3` and an underline color as `58;5;N`. xterm 390 does not parse the colon sub-parameters or 58: it reads `4:3;58;5;9` as blink and strikethrough and loses the underline (the cells print as `0;1;5;9;90;44` for bold, blink, strike). A snapshot (or the overlay's pen restore) of a curly or colored underline is therefore wrong on xterm. Programs normally send curly underlines only when terminfo advertises them (`Smulx`), which `xterm-256color` does not, so this needs a session started in a terminal that does and attached to from an xterm. A guard for the sweep: write `4` instead of `4:3` unless the client's terminfo has `Smulx`.

### 7. Smaller observations

- kitty 0.32 logs `Unhandled charset related escape code` for `ESC * B` and `ESC + B`: it has no G2 and G3. The snapshot writer designates all four sets to ASCII first; on kitty the two extra designations are ignored (the screen is correct).
- kitty's cursor position report is wrong in origin mode (above), and it supports `?2026` (it logs a "pending mode stop" when a bracket is closed after its timeout, which the `as-sync-output` trace provokes on purpose).
- WezTerm 20240203 puts the cursor one column past the right edge for `CSI 999;999 H` (the character lands outside the grid and stays in `get-text`); fil's writers never move the cursor past the edge.

### 8. Partial scroll regions: tmux's history, and WezTerm's intermittent behavior

`ov-scroll-region`. tmux saves lines scrolled out of a partial scroll region into its history; the screen is identical, so only a test that compares scrollback sees it. **WezTerm 20240203 is not deterministic here**: a line feed at the bottom margin of the region sometimes moves the cursor out of the region instead of scrolling it (the same bytes, run after run, give either answer; a settle delay before the trace does not change it), and a line feed below the region scrolls the whole screen where the model and the other terminals scroll nothing. The cause was not isolated (whether the overlay's cursor hide and paint around the line feeds matter, or WezTerm alone), so the trace is listed as known for WezTerm; it is the only intermittent one in six full runs, and the first thing to look at if it is ever worth knowing.

## What to do with the findings

| Finding | Package | Change |
|---|---|---|
| 1 | W1 (and E1a) | W1 goes ahead; the repaint renders the whole screen; no guess is written between a resize and the repaint (rule 9). The reflow is not a thing the core can predict |
| 2 | C1 / P2 (the holder of `tracker.rs`) | After DECSTR the saved cursor is unknown (the cursor after a later DECRC is not trusted) |
| 3 | W1 | Use the row-by-row clear (`CSI 2 K` per row, then home) in the repaint, or accept one screen of tmux scrollback per repaint and say so in `DESIGN.md` §6.2; the same question for the snapshot writer's own clear |
| 4 | K1 | Remember that the shadow's history is not a terminal's |
| 5 | P1 | The disputed-width list above |
| 6 | the sweep | `4:3` and `58` only with `Smulx` |

## Running it

```bash
tests/terminals/run.sh                      # every installed terminal; needs Xvfb, python3, cargo
tests/terminals/run.sh tmux xterm           # only these
tests/terminals/run.sh kitty -- --only ov-pen --dump   # narrow by trace name; keep each raw read-back
python3 -I tests/terminals/report.py target/terminals  # the tables above, from the results
gh workflow run linux-check -f ref=BRANCH -f suite=terminals   # on GitHub (suite=all includes it)
```

Rerun it whenever `overlay.rs` changes a byte it writes, whenever the snapshot writer changes, and at the sweep close (`docs/m3/plan.md` §5.3). It is on demand and not part of `just ci`.

In the local Lima VM (`docs/dev/vm.md`), install the terminals once, visibly (kitty and WezTerm need `libgl1-mesa-dri`; the WezTerm package is the upstream `.deb` for Ubuntu 22.04 and arm64, whose SHA-256 is not published by upstream, so the VM copy is not hash-checked; the CI job pins the x86_64 one):

```bash
limactl shell fil -- sudo apt-get install -y xvfb xterm kitty xdotool x11-utils libgl1-mesa-dri libegl1 libgl1
limactl shell fil -- sudo apt-get install -y ./wezterm-20240203-110809-5046fc22.Ubuntu22.04.arm64.deb
just vm-run tests/terminals/run.sh
```

### Run times

89 traces, about 520 checkpoints per terminal.

| | xterm | kitty | WezTerm | tmux | Whole job |
|---|---|---|---|---|---|
| GitHub runner, `linux-check -f suite=terminals` (run 41801078111, x86_64) | 125 s | 108 s | 44 s | 17 s | 5 min 42 s, with the apt packages, the WezTerm download and a cached cargo build |
| Lima VM (aarch64, 6 cores) | 134 to 140 s | 64 to 246 s (varies with other work in the VM) | 47 to 112 s | 16 to 33 s | about 5 to 9 min |

xterm is the slowest because each scrollback read sends a key with `xdotool` and waits for the file; the rest is the cost of a fresh terminal per trace (about 0.5 s for kitty and WezTerm).

Workflow runs of this branch: 41797985187 (WezTerm failed to start for want of `libEGL`; xterm and kitty were green), 41798691924 (green except two checks of the intermittent WezTerm scroll-region trace), 41801078111 (green).

## Files

- `tests/terminals/run.sh`: the entry point. `run.py` starts a terminal per trace, `player.py` runs inside it, `ansi.py` parses the read-back, `report.py` prints the tables, `known.tsv` lists the known disagreements.
- `tests/terminals/exporter/`: the trace exporter, a standalone cargo workspace with its own `Cargo.lock`. It was planned as `crates/fil-predict/examples/trace_export.rs`; it lives here because it is a harness, not an example of the crate, and so that T2 touches nothing in `crates/`.
