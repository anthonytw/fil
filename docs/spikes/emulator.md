# Spike A: emulator restore fidelity

Question (DESIGN.md §6.5, §16.2): which headless emulator should fil use by default, judged by how
faithfully a snapshot restores a session (§6.2) rather than by taste?

Candidates built: `alacritty_terminal` 0.26.0 (Apache-2.0) and `vt100` 0.18.1 (MIT). libghostty-vt is out of scope.

## Recommendation

**Make `alacritty` the default backend; keep `vt100` compiled in as the small alternative.**
The owner decides; DESIGN.md is not edited here.

- On the 29-fixture corpus the generic snapshot writer restores **every field alacritty can report** (0 failures in
  870 field checks per scenario, 4 scenarios). vt100 also has 0 failures there, but it can report far fewer fields
  (189 of 870 are "unsupported" against 29), so its pass rate says less.
- The **continuation probe** (feed the same follow-up bytes to the original and the restored terminal) is where the
  difference shows. After a restore, vt100 loses the saved cursor, origin/autowrap/insert state, and (while the
  alternate screen is up) the whole scrollback. alacritty loses only tab stops and the charset designation.
- alacritty is right in the majority of the adjudicated cross-backend disagreements (below). vt100 mis-parses SGR 58
  and colon sub-parameters, has no DECAWM, handles backspace in the pending-wrap state differently from xterm, and has
  no DEC graphics set.
- alacritty is the only one with reflow, hyperlinks, underline styles, and query replies (DA, DSR, kitty query).
- It is also faster, about 1.9x the parse throughput of vt100. It costs about 30% more memory per session and
  needs more glue (below).
- Risk to weigh: `alacritty_terminal` is a library extracted from an application. Its API is not designed for
  this use (see "API and maintenance"), so every upgrade needs a re-run of this harness.

## Method

```
fixture bytes --feed--> emulator --state(1000)--> S1 --snapshot()--> bytes --feed--> fresh emulator --> S2
                                                    \________________ compare(S1, S2) ________________/
```

- `fil_term::snapshot::snapshot` is one generic writer over `TermState`. It writes history lines, then the screen,
  then the modes of DESIGN.md §6.2 (alternate screen with the primary behind it, cursor position, shape and
  visibility, pending wrap, pen, mouse, bracketed paste, focus, keypad, kitty flags, scroll region, title,
  autowrap/origin/insert), plus OSC 8 hyperlinks and the DECSC saved cursor. Control characters in cell text, titles
  and link targets are stripped before they are written (unit-tested), so a snapshot cannot smuggle escape sequences to
  the client's terminal.
- `fil_term::compare::compare` scores 30 named fields. A field outside the backend's declared `Capabilities` is
  scored **unsupported**, not pass or fail.
- Per fixture and backend, four scenarios:
  1. **round-trip** at the original size;
  2. **narrow** (two thirds of the columns): resize, then round-trip. Also checks that the text of the logical
     (unwrapped) lines survived the resize;
  3. **wide** (one and a half times the columns): same;
  4. **probe**: after the original stream and after the restored one, feed a fixed follow-up
     (`ESC[2;1H lqk TAB T ESC 8 Z ESC[?6h ESC[999;1H R ESC[?6l`). It prints through the charset, a tab stop, the
     saved cursor and the scroll region, so it exposes state that `TermState` does not carry or a backend cannot read.
- **Cross-backend**: compare the two backends' S1 on the original stream; **cross-restore**: render A's snapshot
  and feed it to B (a stand-in for "another terminal reads the bytes").
- Corpus: 24 synthetic streams (written as Rust source in `tests/common/synthetic.rs`, generated to `.raw` by
  `just fixtures`, checked for drift by a `just ci` test) and 5 real recordings made by `just record` on a PTY
  with a scrubbed environment: bash with colored `ls`, vim editing a Python file, `less`, `top`, and a `\r`
  progress bar. Recorded on macOS 27.2: bash 3.2, Vim 9.1, less 668, macOS `top`. In `top_view`, process names
  and the clock are replaced with placeholders by the recorder (escape sequences untouched); all fixtures were
  grepped for usernames, hostnames, paths and IPs.
- `just ci` includes a fast version of scenarios 1-3 over the whole corpus (0.2 s). `just fidelity` runs everything
  plus the replies, performance and memory sections. A second gate in `just ci` checks that reading the state
  is harmless: for every fixture and backend, `state()` twice gives equal results, and an emulator that has been
  read and then fed follow-up bytes ends in exactly the state (and replies) of a never-read one.

## Results

All numbers are from `just fidelity` on an Apple Silicon Mac (release build). Cells are `pass/fail/unsupported` field
counts summed over the 29 fixtures (29 x 30 = 870 per row).

| Scenario | alacritty | vt100 |
|---|---|---|
| round-trip | 841 / **0** / 29 | 681 / **0** / 189 |
| narrow (reflow) | 841 / **0** / 29 | 681 / **0** / 189 |
| wide | 841 / **0** / 29 | 681 / **0** / 189 |
| probe (continuation) | 840 / **1** / 29 | 676 / **5** / 189 |
| narrowing keeps logical line text | 26 of 26 fixtures (3 skipped: alt screen) | 16 of 26 (no reflow) |
| cross-restore (A's snapshot fed to B) | alacritty to vt100: 4 diffs | vt100 to alacritty: 3 diffs |

Probe failures (the only ones):

| Fixture | alacritty | vt100 |
|---|---|---|
| `hidden_state` (DECSC, G0 left in DEC graphics, custom tab stops) | G0 charset not restored (observed; tab stops are not restored either, by construction; neither is in the model) | saved cursor lost |
| `alt_exited` | | saved cursor (the 1049 restore point) lost |
| `edit_ops` | | saved cursor lost |
| `scroll_region` | | the fixture leaves origin mode on and vt100 cannot report it, so the probe lands elsewhere (my reading of the diff, not isolated further) |

The cross-restore differences are the emulator disagreements adjudicated below (SGR 58, `4:2`, VS16 width, X10 mouse).

Fields counted unsupported, per backend: alacritty reports everything except SGR blink (it ignores blink).
vt100 does not report blink, hidden/strikethrough, underline styles or colors, hyperlinks, autowrap/origin/insert,
or the saved cursor; with the alternate screen active it also reports no primary screen and no history.

### Capabilities declared

| | alacritty | vt100 |
|---|---|---|
| reflow | yes | no |
| hyperlinks (OSC 8) | yes | no |
| underline styles and color | yes | no |
| blink / hidden / strikethrough | no / yes / yes | no / no / no |
| kitty keyboard flags | yes (native) | yes (hand-tracked from unhandled-CSI hook) |
| title | yes (event) | yes (callback) |
| focus events (`?1004`) | yes | yes (hand-tracked) |
| autowrap / origin / insert | yes | no |
| scroll region | yes (via sidecar parser) | yes (via sidecar parser) |
| primary screen behind alt, with its cursor | yes (via `swap_alt` dance) | no |
| saved cursor (DECSC) | yes | no |
| scrollback readable while on alt screen | yes | no |
| pen, pending wrap, cursor shape | yes | yes |
| query replies | yes | no |

### Query replies (80x24, cursor at row 5, col 7)

| Query | alacritty | vt100 |
|---|---|---|
| DA1 `CSI c` | `CSI ? 6 c` | none |
| DA2 `CSI > c` | `CSI > 0;2600;1 c` | none |
| DSR `CSI 5 n` | `CSI 0 n` | none |
| CPR `CSI 6 n` | `CSI 5;7 R` | none |
| kitty `CSI ? u` | `CSI ? 0 u` | none |
| kitty after `CSI > 5 u` | `CSI ? 5 u` | none |

vt100 has no reply support at all: those sequences reach its unhandled-sequence hooks, and answering would be code
fil writes, about 30 lines for these five. alacritty's `ColorRequest` (OSC 10/11 queries, which vim sends; see
`vim_edit`) and `TextAreaSizeRequest` events arrive as closures that need the host's answer; the backend ignores
them today, so they are **not** answered. M1 must answer them or programs that ask will wait.

*Update (M1, `fil-session`):* the session answers them itself while detached (`fil-session/src/queries.rs`), from
the colors the last attached terminal reported or dark-theme defaults.

### Performance and memory

Release build, one thread. The stream is 16.8 MB of synthetic mixed text, SGR changes and erases, so only the
ratio is meaningful.

| | alacritty | vt100 |
|---|---|---|
| parse throughput (best of 3) | 133 MB/s | 72 MB/s |
| same, without the scroll-region sidecar | 183 MB/s | 84 MB/s |
| `state(1000)` at 200x50 | 2.4 ms | 3.3 ms |
| `snapshot()` of that state | 2.4 ms, 127 KB | 2.4 ms, 127 KB |
| memory per session, 200x50, 1000 history lines | 5.6 MB | 4.3 MB |

The sidecar (a second `vte` pass that tracks DECSTBM) costs alacritty about 27% of its throughput. It is needed
because neither crate exposes the scroll region.

## Cross-backend disagreements, adjudicated

Nine of 29 fixtures differ in at least one supported field (20 are identical). Each row is a real difference
found by the harness; the verdict comes from the standard text (ECMA-48 / the xterm ctlseqs) or from xterm's
documented behavior, **not** from running xterm or another reference terminal here.

| # | Behavior | alacritty | vt100 | Right |
|---|---|---|---|---|
| 1 | BS while a wrap is pending (`abcde BS X` in 5 cols). xterm/DEC: the cursor sits on the last column, BS moves it left | `abcXe` | `abcdX`, wrap still pending | alacritty |
| 2 | DECAWM off (`CSI ? 7 l`) and 25 chars into a 20-col line: last column is overwritten, no wrap | overwrites | wraps (no DECAWM; the line gets a wrap flag) | alacritty |
| 3 | SGR 58 with an indexed color (`4;58;5;33`) | underline colored | treats `5` as blink and `33` as yellow foreground | alacritty (vt100 corrupts the foreground) |
| 4 | Colon sub-parameters: `38:2::10:200:30`, `4:2` (ITU T.416 / kitty form) | parsed | ignored | alacritty |
| 5 | DEC special graphics (`ESC ( 0`) | `┌─┐` | literal `lqk` | alacritty |
| 6 | Insert mode (`CSI 4 h`) | honored | ignored | alacritty |
| 7 | X10 mouse reporting (`CSI ? 9 h`) | not supported (mode stays off) | honored | vt100 |
| 8 | EL/ECH with bold set: does the erased cell keep bold? xterm erases with the background color only | bold dropped | bold kept on the blank cell | alacritty (invisible on screen) |
| 9 | ED 2 (`CSI 2 J`): ECMA-48 and xterm erase, nothing goes to scrollback | pushes the visible lines into history (observed: 2 lines after `a CRLF b CRLF CSI 2J`; 1 on an empty screen) | erases only | vt100 by the letter; alacritty's behavior looks deliberate and is what many users expect from `clear`, but it changes what "history" means for a snapshot |
| 10 | VS16 emoji presentation (`❤` U+FE0F) width | 1 cell | 2 cells | unresolved: terminals disagree (xterm 1, kitty/WezTerm/iTerm2 2). Width has to match the user's real terminal, which fil cannot know |
| 11 | Alternate screen via `?47` / `?1047` (older terminfo entries use these) | only `?1049` is recognized (read from vte's mode list), so these are ignored | `?47` honored, `?1047` ignored | neither; xterm supports all three. Found by reading the sources, not by a fixture |

Alacritty-side artifacts that are not disagreements, normalized in the adapter: it stores a literal TAB in cells it
tabbed over (mapped to a blank), a leading blank filler before a wide character that did not fit at a line end, and
generated ids for hyperlinks without one (dropped).

## API and maintenance

**alacritty_terminal.** Releases: 0.23.0 (2024-03), 0.24.0 (2024-05), 0.25.0 (2025-02), 0.25.1 (2025-10),
0.26.0 (2026-04), plus release candidates; MSRV 1.85. Three minor bumps in about 25 months; I integrated against
0.26.0 only and did not diff earlier releases, so I cannot say how much each bump broke. It is an application's core
exposed as a crate: the grid (`Grid`, `Cell`, history, cursor, `input_needs_wrap`, the cursor template) is public, the
terminal's other state mostly is not. Glue that was needed (`src/be_alacritty.rs`, 438 lines in total):

- the **inactive grid is private**. `Term::swap_alt` is public, so `state()` swaps to the primary, reads it, swaps back
  and writes the alternate grid back from a copy. That works (round-trip and probe pass on `alt_active`, `vim_edit`,
  `less_view` and `top_view`), but a read mutates the terminal, so the `Term` sits in a `RefCell`. `swap_alt` has side effects beyond swapping, and a first version of this adapter missed one: it re-derives the kitty
  flags from the top of the flag stack, but `CSI = f u` changes the flags without touching the stack, so a read
  on the alternate screen silently reset them (caught in review; the follow-up bytes then ran with the wrong flags).
  The adapter now puts the flags back after the second swap. The other effects are harmless here: the second swap
  copies the primary cursor into the primary's saved cursor, which is what `1049 h` (alacritty's only way into the
  alternate screen) had stored already and the primary cursor cannot move meanwhile; selection and damage tracking
  are reset, and nothing here uses them. The idempotence gate above covers this class of bug; with the fix, it
  found nothing else on any fixture or backend;
- **scroll region private**: the sidecar `vte` parser (`src/sniff.rs`, 115 lines);
- **cursor style**: `Term::cursor_style` folds "unset" into a configured default, so the adapter sets a sentinel
  default (hollow, blinking) to tell "never set" from "steady block";
- **title** from events; **replies** from `Event::PtyWrite`;
- **kitty keyboard**: off unless `Config::kitty_keyboard` is set;
- the `serde` feature is not needed (`default-features = false`); it still pulls `libc`, `polling`, `rustix-openpty`
  and Windows crates for its PTY module, which fil does not use.

**vt100.** The crate sat at 0.16.2 (2025-07) and then released 0.17.0, 0.18.0 and 0.18.1 on 2026-10-18/19, two weeks
before this spike, so 0.18 is new and I read its source rather than relying on documentation. It is small
(about 5 KLOC), has a `Callbacks` trait for unhandled sequences, and screen contents are cheap to read. But the
snapshot model needs things it does not store or expose: no hyperlinks, underline styles, strikethrough/hidden/blink,
autowrap, insert, or the saved cursor; the other grid is private with no `swap` equivalent; and scrollback is read by
moving the view (`set_scrollback`), which on the alternate screen reads the alternate grid, so history is unreadable
there. Glue (`src/be_vt100.rs`, 321 lines): title, focus (`?1004`) and a kitty flag stack are rebuilt by hand from the
`unhandled_csi` hook (about 50 lines), plus the same scroll-region sidecar.

**shpool's fork.** `shpool_vt100` 0.1.3 (MIT, GPL-3-compatible; last release 2025-02) is an older fork of vt100. From its
source (I did not build it) it adds `title()`, `icon_name()` and `last_n_rows_contents_formatted`, but has no
`scrollback_len` and the same missing fields: no hyperlinks, underline styles, strikethrough, or access to the other
grid. Upstream 0.18 covers what the fork adds (title via callback), so the fork is **not used**.
shpool's README says it is replacing the fork with `shpool-vterm` (Apache-2.0 AND MIT, OK for deny.toml). That crate
exposes only a dump (`Term::contents(region) -> Vec<u8>`), no cell access, so it cannot sit behind the `TermState`
model; I did not evaluate its restore quality.

## Model changes (PR #3 `fil-term` types)

- `Cursor.pen: Attrs`: the SGR pen, restored at the end of a snapshot.
- `TermState.primary_behind_alt` is now `Option<PrimaryScreen { screen, cursor }>` (was `Option<Vec<Line>>`): the
  `1049 l` restore point needs the primary cursor.
- New `SavedCursor { row, col, pen }` and `TermState.saved_cursor: Option<SavedCursor>` (DECSC).
- `Capabilities` grew from 5 flags to 18: `blink`, `strike_hidden`, `primary_behind_alt`, `scroll_region`, `title`,
  `focus_events`, `line_modes`, `pending_wrap`, `pen`, `saved_cursor`, `history_in_alt`, `cursor_shape`,
  `mouse_encoding_extra`.
- Constructors and defaults (`Cursor`, `Modes`, `Cell::blank`, `Line::blank`, `TermState::blank`, `Line::text`,
  `TermState::screen_text`); `Modes` has a manual `Default` (autowrap on).
- Conventions documented in `model.rs`: blank cells have empty text; wide-char spacers are width 0; hyperlink ids a
  backend invented are dropped.
- The `Emulator` trait is **unchanged**. Both adapters implement `state(&self)` with a `RefCell`, which is a sign the
  trait may want `&mut self` for `state`.

## What this does not show

- Only emulators read the snapshot. No real terminal (Terminal.app, iTerm2, kitty, a Linux terminal) was tried,
  so how a given terminal handles `4:3`, `58`, `CSI > u` or `OSC 8` is unverified. The cross-restore rows show the
  hazard: vt100 mangled the foreground color on `58;5;n`. A real client may need a writer option to leave out
  sequences its terminal does not support.
- **A snapshot taken while a program's escape sequence is half-received** loses the pending bytes: neither crate
  exposes parser state. The daemon should snapshot at a sequence boundary or retain the unparsed tail.
- Not restored by the writer: tab stops, charset designations (G0-G3, shifts), the primary cursor's pending wrap
  behind the alternate screen. The probe shows all three.

*Update (M1, `fil-session`):* the session holds back the unfinished end of the output so a snapshot is always taken
at a sequence boundary (`fil-session/src/boundary.rs`). `fil-term` now carries tab stops and charsets (G0-G3, the
invoked set, and the ones DECSC saved) in `TermState` and the writer, for alacritty; the probe passes for it
(`just fidelity`: alacritty 899/0/29). vt100 does not report them (`Capabilities::tab_stops` and `charsets` are
false). alacritty keeps a program's synchronized-update batch (`?2026`) until it ends, so `Emulator::apply_pending`
applies it before a snapshot is rendered. The primary cursor's pending wrap is still not restored.
- Not run: fuzzing, chunk-split feeding (a single `feed` per fixture), non-UTF-8 input, sixel/kitty graphics, a
  Linux host. Real recordings are macOS programs.
- Perf and memory are one machine, one synthetic stream. RSS per session is a delta over 40 sessions and includes
  allocator slack.
- Adjudications use the standards text and my knowledge of xterm's behavior. I did not run xterm.

## Reproduce

```
just fidelity                # all sections; --only NAME, --no-perf, --verbose
just fixtures                # regenerate synthetic .raw files after editing tests/common/synthetic.rs
just record [scenario ...]   # re-record real fixtures (inspect them for personal data first)
just ci                      # includes a fast round-trip gate over the corpus
```

Code: `crates/fil-term/src/{snapshot,compare,sniff,be_alacritty,be_vt100}.rs`, `examples/{fidelity,fixtures,record}.rs`,
`tests/{roundtrip,fixtures_current}.rs`, fixtures in `tests/fixtures/`.
