# Real terminals (M3, package T2)

> **Status: built and run on 2026-11-04** (`tests/terminals/`, `linux-check -f suite=terminals`). The plan is `docs/m3/plan.md` §3 "T2" and §5.3. `DESIGN.md` stays the source of truth; this file is the evidence behind the changes the findings ask for.

Until now the prediction core, the snapshot writer and the status line were checked against `alacritty_terminal` and `vt100` only. This suite replays what fil writes into **real terminals**, reads their screens back by the terminals' own commands, and compares each screen with the shadow model (`alacritty_terminal` through `fil-term`, the emulator the daemon and the client's shadow screen run). Every difference is reported as a **disagreement between the model and a terminal**. On the screen, that is corruption of the user's view, not a misprediction.

## What is replayed

`tests/terminals/exporter` (its own cargo workspace; not part of `just ci`) writes one JSON trace per scenario. A trace is a list of steps: bytes to write, resizes, and checkpoints with the expected screen, cursor, cell attributes and scrollback taken from the model fed the same bytes. 94 traces, about 560 checkpoints:

| Group | Traces | What the terminal is fed |
|---|---|---|
| `ov-*` | 21 hazard traces (the spike's 20 and `ov-pen-colon`; `crates/fil-predict/tests/hazards.rs`, one per row of the hazard table), each with synchronized output on and off; `ov-typing`, a typing session with the default predictor at 200 ms | The real `Overlay`'s output: the erase, the server bytes, the paint, and the `?2026` markers, with a checkpoint after each flush and, where the erase is at rest, right after the erase |
| `st-*` | 6 status-line scenarios (`crates/fil-predict/tests/status.rs`), with and without synchronized output | The same, for the pinned status (inverse video) |
| `sn-*` | 9 snapshots: plain, history, wrapped lines, styles, wide characters, alternate screen, scroll region with origin mode and tab stops and DEC graphics, pending wrap, autowrap off | `fil_term::snapshot::snapshot` of a state, then the probe |
| `as-*` | 11 micro-tests of the assumptions `docs/spikes/prediction.md` lists as unverified, and `as-scroll-region`, the overlay-free control for `ov-scroll-region` | Hand-written bytes |
| `wd-widths` | 30 strings | Each printed at the start of a row; the cursor position report says how wide the terminal thinks it is |
| `rf-*` | 7 reflow cases | Wrapped lines, history, a prompt with typed text, wide characters at the edge, a snapshot; narrowed, widened, returned |
| `cl-*` | 8 clears | A screen with scrollback, then `CSI H CSI J` (the snapshot writer's clear), `CSI H CSI 2 J`, `CSI H CSI 2 K CSI 2;1 H CSI J` (an erase to the end that does not start at home) or row-by-row `CSI 2 K`; and the same over a short session |
| `canary` | 1 | A deliberately wrong expectation (another row text, another cursor, other attributes). The run fails unless all three are reported: a blind read-back must not pass |

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

- A cursor position report with a **wrap pending** is `(row, cols+1)` in tmux and `(row+1, 1)` in kitty; each is accepted on its own terminal only, when the model has a pending wrap.
- In **origin mode** tmux reports the cursor absolute, and WezTerm does in some runs (xterm and, mostly, WezTerm report it relative to the region, as the standard says); both accept either. kitty's report is wrong by a constant (off by one in a plain case, by two in another), so the cursor is not compared on kitty in origin mode; the probe shows where the cursor really is.
- **xterm's print-screen** does not write a cell's attributes across blanks and does not go back to the default after an attributed run, so the attributes of blank cells are not compared on xterm, and neither is a cell that has exactly the flags of the cell before it unless the model expects them. A bug that spread an attribute to the *next* cell would not be seen on xterm.
- **WezTerm's** `get-text --escapes` prints no curly underline, so the curly cells of `sn-styles` and `as-pen` cannot be read back; and `get-text` ends with an extra empty row.
- Inside a synchronized-output bracket **WezTerm holds its replies** until the end marker (no cursor report in two seconds); the check is informational.
- xterm's print-screen **precomposes combining marks** (`e` + U+0301 reads back as `é`) and **drops U+200D**; the `sn-wide` differences on xterm are that, not a width verdict.
- After DECSTR the model's reports are inconsistent: `fil-term`'s sniffer drops the *reported* scroll region while alacritty keeps scrolling inside the old one (finding 2). That is a fil-term gap, not a read-back quirk.

## Results

`tests/terminals/known.tsv` lists every disagreement the run knows about: the terminal, the checkpoint, a reason, and a hash of the **exact differences** found there (their text is deterministic). A disagreement passes only if it is the listed one. An unlisted checkpoint fails, a listed checkpoint with a different or an added difference fails (so a new bug at a checkpoint that already has a known difference is not hidden), and a listed difference that no longer occurs is reported as `STALE` and fails. The one exception is a `trace/*` wildcard for an intermittent trace (WezTerm's scroll region). `TERMINALS_STRICT=1` ignores the list. Reasons tagged `[fil-term fix PR: ...]` are fil-side bugs the follow-up fix PR removes, not terminal quirks. Today: **no unlisted or changed disagreement, no stale entry, no read-back error, the canary caught on all four.**

Run 41810202004 on GitHub (before `ov-pen-colon` was added; the counts include the canary's checkpoint only as agreeing):

| Terminal | Agree | Disagree (all listed) | Informational |
|---|---|---|---|
| xterm | 462 | 67 | 0 |
| kitty | 488 | 40 | 1 (the cursor report held inside a `?2026` bracket) |
| WezTerm | 472 (470 to 474 across runs: the intermittent traces) | 56 | 1 |
| tmux | 498 | 31 | 0 |

**The overlay's own writes agree on all four terminals**, with synchronized output on and off, at every flush checkpoint and at every erase point where the erase is at rest, for the 20 hazard traces, the typing session and the 6 status-line scenarios, with these exceptions: across a resize (finding 1); on xterm, where the pen restore and the program's own pen carry `58;5;N` and `4:3`, which xterm 390 misreads (finding 6); on WezTerm, where `get-text` cannot show a curly underline (a read-back limit, not a verdict); at the probe after `ov-origin` on every terminal (finding 2: the shadow, not the terminal, is wrong); and WezTerm's intermittent partial scroll region trace (finding 8). That is the answer to "can the erase be wrong on a real terminal": not outside a resize, on what was measured.

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

### 2. DECSC/DECRC and DECSTR: fil-side gaps in the shadow, measured on real terminals

`ov-origin*`, `ov-decstr*`, `ov-restore-without-save*`, `as-decstr`, `as-restore-without-save`, `sn-region-tabs-charset`. The spike's hazard table (row 7 and the DECSC note) listed these as "terminals disagree"; the measurement says the terminals mostly agree with each other and the **shadow is the odd one out**, so they are fil-term gaps, tagged in `known.tsv` for the fix PR:

- **DECRC and origin mode.** All four terminals save and restore origin mode with DECSC/DECRC; alacritty does not save it, and the snapshot writer takes its own DECSC with origin mode off. A program's later `ESC 8` therefore turns origin mode off on every real terminal and leaves it on in the shadow. The tracker marks origin mode unknown after a restore, so nothing is painted in that state, but the shadow's cursor and screen are wrong from there. Fix: track DECSC's origin bit in `fil-term` and make the snapshot writer's DECSC reproduce it.
- **DECSTR.** vte 0.15 has no `CSI ! p` handler, so the shadow ignores it. xterm and WezTerm reset the character sets, insert mode, origin mode and the saved cursor; tmux resets the character sets; kitty ignores DECSTR too (and agrees with the shadow). `xterm-256color`'s `is2` and `rs2` contain `\E[!p`, so `tput init` sends it. After it the snapshot writer re-applies the shadow's insert mode, origin mode and character sets to the user's terminal. The shadow can also hold a **saved cursor** the terminal dropped (`as-decstr/saved-cursor`: a program that does DECSTR and then DECRC lands at its old position in the shadow and at home on xterm and WezTerm), and the tracker does not list the saved cursor as unknown after DECSTR: a **small new tracker guard** (by whichever package holds `tracker.rs` then). Fix in `fil-term`: model DECSTR.
- `CSI ? 1049 l` with nothing saved restores (and resets the charset) on xterm and tmux and is ignored by alacritty, WezTerm and kitty. The tracker already marks the charset unknown; not a fix-PR item.
- kitty keeps the current pen across a restore with nothing saved (terminal-side).
- WezTerm after the probe's `ESC 8` with nothing saved: the screen scrolls as if the scroll region were gone (`ov-origin` on WezTerm shows `hi!` where the others show origin-mode differences). Probably DECRC resets the margins in WezTerm; not isolated.

### 3. The snapshot writer's clear pushes the screen into the scrollback on tmux (W1's clear)

`cl-*`. Scrollback lines after the clear, over a full screen with 32 lines of scrollback (the "short" row is a screen holding three lines and no scrollback):

| Clear | alacritty (model) | xterm | kitty | WezTerm | tmux |
|---|---|---|---|---|---|
| `CSI H CSI J` (the snapshot writer's), full screen | 32 | 32 | 32 | 32 | **55** |
| `CSI H CSI J`, short session | 0 | 0 | 0 | 0 | **3** |
| `CSI H CSI 2 J`, full screen | **55** | 32 | 32 | 32 | **55** |
| `CSI H CSI 2 J`, short session | **3** | 0 | 0 | 0 | **3** |
| `CSI 2 K` on each row, then `CSI H` (full / short) | 32 / 0 | 32 / 0 | 32 / 0 | 32 / 0 | 32 / 0 |

- **tmux pushes the visible screen into its history on `CSI H CSI J`**, and the visible lines of a short session too. A snapshot (attach, catch-up, resume) and W1's repaint therefore cost one screen of tmux scrollback each time. The cause is tmux's `scroll-on-clear` option (on by default since 3.4): with `set -g scroll-on-clear off` the same traces agree with the model on `CSI H CSI J` and on `CSI 2 J` (measured with `TERMINALS_TMUX_CONF`). That is the user's lever; fil cannot set it.
- **Two clears push nothing on any of the four, and the model agrees**: `CSI 2 K` on each of the 24 rows then `CSI H` (`cl-rows`), and `CSI H CSI 2 K CSI 2;1 H CSI J` (`cl-ed0-nonhome`: an erase to the end of the screen that does not start at home; three sequences, 32 and 0 lines before and after on all four, measured). Either is the clear W1 and the snapshot writer can use if they want one that is "safe on all four terminals". (The pen is reset first, as the snapshot writer already does, so the erase fills with the default background.) The three-sequence form is the cheaper.
- `CSI 2 J` is not a substitute: tmux pushes on it too, and the model does as well (next finding).

### 4. The shadow pushes a cleared screen into its history; xterm, kitty and WezTerm do not

`cl-ed2`, `ov-clear-screen`. On `CSI 2 J` alacritty copies the screen into its scrollback (55 lines in the table above), and xterm, kitty and WezTerm keep their scrollback as it was (32). The daemon's snapshot history is rendered from the shadow, so after a program clears the screen with `CSI 2 J` the snapshot's history contains the cleared screen and the user's own scrollback does not. The next attach of a client with a clean terminal shows lines it never had. Harmless for the screen; worth knowing for K1's catch-up decision and for any test that compares history with a terminal's. `CSI H CSI J` is the other way round: the model does not push, tmux does.

### 5. Widths: where `unicode-width`, the shadow and the terminals disagree

`wd-widths`. Columns the cursor advances after printing the string at the start of a row. A `*` marks a row where the five columns are not all equal.

| String | `unicode-width` (the crate, on the whole string) | alacritty (shadow) | xterm | kitty | WezTerm | tmux |
|---|---|---|---|---|---|---|
| ASCII, Latin-1, Latin Extended-A, combining acute, Greek, box drawing, middle dot, plus-minus, curly quote, black star, circled 1 (the "ambiguous" ones), check mark, halfwidth katakana | 1 | 1 | 1 | 1 | 1 | 1 |
| CJK, fullwidth, Hangul, emoji with default emoji presentation, a regional-indicator pair, ZWSP in `x​y` | 2 (pair: 2; ZWSP string: 2) | same | same | same | same | same |
| `U+2764 U+FE0F` (heart VS16), smiley VS16, `1 U+FE0F`, keycap | **2** | **1** | 1 | **2** | 1 | **2** |
| thumbs up with skin tone | 2 | **4** | **4** | 2 | 2 | 2 |
| ZWJ family (three emoji joined) | 2 | **6** | **6** | **6** | 2 | 2 |
| `x U+00AD y` (soft hyphen) | 2 | 2 | **3** | 2 | 2 | **3** |
| a lone regional indicator | 1 | 1 | 1 | **2** | 1 | **2** |

- **Ambiguous-width characters are not a problem on these four**: every one measures 1 everywhere.
- **VS16 sequences, skin tones, ZWJ sequences, the soft hyphen and a lone regional indicator are disputed.** `unicode-width` applied to a whole string is *not* the shadow's table (it says 2 for VS16 and for the skin-tone and ZWJ strings where alacritty says 1, 4 and 6); the overlay itself only measures single characters (`predictable()` admits printable ASCII and Latin letters, via `UnicodeWidthChar`), so the column marked `unicode-width` is the crate's answer for the string, not something the overlay computes, and the terminals split three ways. A cell restored from the shadow after the client has painted next to such a sequence can land one column off. `crates/fil-predict` paints only printable ASCII and Latin letters, which are 1 everywhere, so the core is safe by construction; the **P1 rule** "no guess next to a cell T2's table marks as disputed" needs the list: any cell whose text contains `U+FE0F`, `U+200D`, a skin-tone modifier (`U+1F3FB` to `U+1F3FF`), `U+00AD` or a regional indicator.
- The snapshot writer writes such cells as the shadow's text; the only visible effect seen is an extra blank after a VS16 heart on tmux and a missing one on WezTerm (`sn-wide`).

### 6. fil writes SGR 58 with semicolons; xterm 390 misreads it (and `4:3`)

`sn-styles`, `ov-pen`, `as-pen`. `fil_term::snapshot::sgr`, which the snapshot writer and the overlay's pen restore (`set_pen`) both use, writes the underline color as `58;5;N` and `58;2;R;G;B` **with semicolons**, and a curly underline as `4:3`. A terminal that does not know 58 reads the `5` and the `9` of `58;5;9` as blink and strikethrough (the xterm cells print as `0;1;5;9;90;44`), and xterm 390 reads `4:3` as a black foreground (`sn-styles` reads back `f0`, `ov-pen` `f8`, bold black): the text goes black rather than just losing its underline. alacritty parses the colon form, so a program that sends the safe `58:5:9` has it rewritten by fil into the unsafe form in every snapshot and every erase. `ov-pen` and `as-pen` send the semicolon form from the program itself (and `ov-pen` compares `bfgu`, not `s` and `k`), so they cannot show that part; `ov-pen-colon` does: the program sends `4:3;58:5:9`, and on xterm the cells fil writes after its pen restore (the cell the program writes next, and the guess) read back with blink and strikethrough (`b,f1,g4,k,s`), which the program's own bytes did not cause. This is a fil encoding choice, not an xterm bug: the follow-up writes `58:5:N` / `58:2::R:G:B`, and `4` instead of `4:3` unless the terminal advertises `Smulx`. Programs normally send curly underlines only when terminfo advertises them, which `xterm-256color` does not.

### 7. Smaller observations

- kitty 0.32 logs `Unhandled charset related escape code` for `ESC * B` and `ESC + B`: it has no G2 and G3. The snapshot writer designates all four sets to ASCII first; on kitty the two extra designations are ignored (the screen is correct).
- kitty's cursor position report is wrong in origin mode (above), and it supports `?2026` (it logs a "pending mode stop" when a bracket is closed after its timeout, which the `as-sync-output` trace provokes on purpose).
- WezTerm 20240203 puts the cursor one column past the right edge for `CSI 999;999 H` (the character lands outside the grid and stays in `get-text`); fil's writers never move the cursor past the edge.

### 8. Partial scroll regions: tmux's history, and WezTerm's intermittent behavior

`ov-scroll-region`, `as-scroll-region`. tmux saves lines scrolled out of a partial scroll region into its history; the screen is identical, so only a test that compares scrollback sees it.

**WezTerm 20240203 is not deterministic on traces with a scroll region, origin mode or a pending wrap** (seen on `ov-scroll-region`, `ov-origin`, `sn-region-tabs-charset` and once on `ov-pending-wrap-nosync`). A line feed at the bottom margin of the region sometimes moves the cursor out of the region instead of scrolling it, and at the probe a line feed below the region scrolls the whole screen where the model and the other terminals scroll nothing; the same bytes give either answer from run to run (a settle delay before the trace does not change it). The overlay-free control `as-scroll-region` (the same server bytes, no paint, same checkpoints) agreed in all nine runs, and so did that control with the overlay's cursor hide, SGR, CUP, paint and erase injected as one write. The full trace, where the overlay's flushes and the checkpoints between them are separate writes, differs. So the cause is not isolated: it may be the overlay's writes interleaved with the line feeds, or the checkpoints (a cursor position report and a `get-text` between the flushes). In one run `ov-origin` on WezTerm showed exactly the differences of the "overlay forgets `?6h`" mutation below, then agreed in the next. The harness therefore runs a WezTerm trace again, up to twice, when it shows an unlisted difference, and passes it if one run shows only listed ones (a bug in what fil writes shows in every run; the mutation check was repeated on WezTerm with this rule and still fails); `ov-scroll-region` stays a `trace/*` wildcard because its answer changes between listed ones. A listed WezTerm difference that does not occur in the accepted run is a warning, not a failure. **Do not read any of this as proven harmless to the overlay**: it is the first place to look if a WezTerm user reports garbage in a program that sets a scroll region.

## What to do with the findings

| Finding | Package | Change |
|---|---|---|
| 1 | W1 (and E1a) | W1 goes ahead; the repaint renders the whole screen; no guess is written between a resize and the repaint (rule 9). The reflow is not a thing the core can predict |
| 2 | fil-term fix PR; C1 / P2 (the holder of `tracker.rs`) | `fil-term`: model DECSTR, track DECSC's origin bit and make the snapshot writer's DECSC reproduce it. Tracker: after DECSTR the saved cursor is unknown |
| 3 | W1; fil-term fix PR (the snapshot writer's clear) | Use `CSI H CSI 2 K CSI 2;1 H CSI J` (or the 24-row `CSI 2 K` loop) in the repaint and in the snapshot writer, or accept one screen of tmux scrollback per snapshot and say so in `DESIGN.md` §6.2; users can also set tmux's `scroll-on-clear off` |
| 4 | K1 | Remember that the shadow's history is not a terminal's |
| 5 | P1 | The disputed-width list above |
| 6 | fil-term fix PR | SGR 58 in colon form; `4` instead of `4:3` unless `Smulx` |

## Mutation checks

Does the suite notice what it should? `tests/terminals/mutants.py` rewrites exported traces as if the overlay wrote different bytes (expectations unchanged); the table is what `run.py` said on each terminal with `TERMINALS_STRICT=1` (traces `ov-typing`, `ov-origin`, `ov-pen`, `st-edge` and the ones whose names start with `ov-pen`), in the Lima VM on 2026-11-04.

| Mutation | Result |
|---|---|
| The read-back sees nothing (`TERMINALS_BLIND=1`: every difference is dropped) | The run fails: `ERROR canary/wrong: ... not fully noticed (missing: row, cursor, attributes)` |
| The overlay forgets `CSI ? 6 h` after a paint or an erase (`no-origin-restore`) | 6 failing checkpoints on each of xterm, kitty, WezTerm and tmux, all in `ov-origin` and `ov-origin-nosync` (`e2/flush`, `end/flush`, `probe`). With `known.tsv` in force the probes are reported as "a listed checkpoint, but not the listed difference" and still fail: a pinned difference does not hide it |
| The erase writes `#` where it should restore the cell (`wrong-erase`) | 11 failing checkpoints on each of the four terminals, in `ov-pending-wrap`, `ov-pending-wrap-nosync` and `ov-typing-nosync`. `ov-typing` with synchronized output on is **not** caught: the erase there is inside a bracket and the next checkpoint follows the repaint, so the sync-off twin is the one that checks erases |
| The paint leaves its underline pen on (`no-pen-reset`) | 37 failing checkpoints on xterm, WezTerm and tmux and 39 on kitty, in `ov-typing` and `ov-typing-nosync` |
| A line removed from `known.tsv` (tmux `cl-snapshot-clear/after`) | `DIFF cl-snapshot-clear/after`, the run fails |
| A line that no longer matches added to `known.tsv` | `STALE ... no longer occurring; remove the line`, the run fails |

Run them with `python3 -I tests/terminals/mutants.py NAME TRACE_DIR OUT_DIR`, then `TERMINALS_STRICT=1 python3 -I tests/terminals/run.py TERMINAL OUT_DIR RESULTS` under Xvfb (`TRACE_DIR` is `target/terminals/traces` after a run).

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

94 traces, about 560 checkpoints per terminal.

| | xterm | kitty | WezTerm | tmux | Whole job |
|---|---|---|---|---|---|
| GitHub runner, `linux-check -f suite=terminals` (run 41810202004, x86_64) | 127 s | 120 s | 46 s | 17 s | 6 min 20 s, with the apt packages, the WezTerm download, a cached cargo build and the exporter's lint |
| Lima VM (aarch64, 6 cores) | 143 to 158 s | 64 to 246 s (varies with other work in the VM) | 44 to 112 s | 16 to 33 s | about 5 to 9 min |

xterm is the slowest because each scrollback read sends a key with `xdotool` and waits for the file; the rest is the cost of a fresh terminal per trace (about 0.5 s for kitty and WezTerm).

Workflow runs of this branch: 41797985187 (WezTerm failed to start for want of `libEGL`), 41798691924 (one intermittent WezTerm trace), 41801078111 (green, before the review fixes), 41809456777 (WezTerm's cursor report in origin mode, accepted since), 41810202004 (green, with the pinned `known.tsv`). `ov-pen-colon` was added after it and checked in the VM.

## Files

- `tests/terminals/run.sh`: the entry point. `run.py` starts a terminal per trace, `player.py` runs inside it, `ansi.py` parses the read-back, `report.py` prints the tables, `mutants.py` makes the mutated traces, `known.tsv` lists the known disagreements with the hash of their differences.
- `tests/terminals/exporter/`: the trace exporter, a standalone cargo workspace with its own `Cargo.lock`. It was planned as `crates/fil-predict/examples/trace_export.rs`; it lives here because it is a harness, not an example of the crate, and so that T2 touches nothing in `crates/`.
