# M3 T1 part 1: the full-stack prediction harness, and resize in the true order

> **Status: built; the result below gates W1 (`docs/m3/plan.md` §2.8).** Written against `origin/main` at `c16606b` (wire revision 11), with the M0 predictor and trait as they are on that commit. Part 2 of T1 (the prediction suites and the independent classifier) builds on this.

## 1. What it is

The real `fil-client` engine (`run_with`, with a predictor factory) against a real `fil-session` with a real program on a real PTY, over `fil-netsim` links with latency and jitter (loss on QUIC), on the real `PipeLink` or `QuicLink`. The user's terminal is an emulator that **reflows when it is resized**, and the user is a seeded typist.

```mermaid
flowchart LR
  typist["seeded typist"] -->|keys| term
  subgraph user["the user's seat"]
    term["oracle terminal<br/>an emulator that reflows"]
  end
  subgraph client["fil-client engine, run_with"]
    disp["Display: shadow screen<br/>and overlay"]
    adapter["adapter predictor<br/>(current MoshPredictor)"]
    disp --- adapter
  end
  subgraph links["fil-netsim"]
    pipe["PipeLink over a stream link"]
    quic["QuicLink over datagrams"]
  end
  subgraph daemon["the daemon's seat, in the test"]
    side["Hello, Attach,<br/>SessionHandle::serve"]
    sess["fil-session"]
    pty["real program on a real PTY"]
    side --> sess --> pty
  end
  term -->|"events, size()"| disp
  disp -->|"erase, server bytes, paint"| term
  disp <--> pipe
  disp <--> quic
  pipe <--> side
  quic <--> side
  sess -. "fresh snapshot at the end" .-> snap["scripted client"]
  term -. "compared with" .-> snap
```

| File | What |
|---|---|
| `crates/fil-client/tests/common/oracle_term.rs` | the oracle terminal, the rig, the adapter, the trials. Self-contained, so the test and the example both include it |
| `crates/fil-client/tests/predict_e2e.rs` | the small cases that run in `just ci`; two reproductions are `#[ignore]`d |
| `crates/fil-client/examples/predict_e2e.rs` | the on-demand runs: `just predict-e2e resize\|baseline\|secret\|all` |
| `crates/fil-client/tests/common/mod.rs` | one line: `pub mod oracle_term;` |

`fil-session`, `fil-netsim` and `quinn` are dev-dependencies of `fil-client`. Neither fil crate depends on `fil-client` (`cargo tree -p fil-session -e normal,build,dev` has no `fil-client`), so there is no cycle.

### The oracle terminal

`OracleOut` is the `TermOutput` the client writes to. It feeds an alacritty emulator and, after every write, looks at the screen:

- **guesses written**: cells that are underlined and hold a glyph (nothing else in these programs underlines; the M0 predictor shows its guesses underlined);
- **flashes**: such a glyph that went away without the same glyph being there, plain (a written guess that was erased and not confirmed). Counted between resizes only;
- **secrets**: any character of the reserved alphabet (Latin-1 letters, plan §5.2) in the decoded byte stream, with a character cut between two writes carried over.

`OracleHandle::resize` is the **true order**: the emulator is resized first (and re-wraps what it holds), then the client is told. `resize_late` is the order the M0 harness used: the client is told, answers, and only then does the emulator change size. It is kept as a control. Both wait for the client to read the new size (`OracleOut::size` is how the engine learns it) before anything else happens.

### What "damage" is measured against

Two references, taken once the run is quiet:

1. **The client's shadow screen** (`Rig::compare_to_shadow`). The overlay's contract is that the user's terminal equals the shadow whenever nothing is drawn. The adapter predictor is handed the shadow on every call, so after the run settles the harness types one more key and reads the shadow from that call (the terminal's state is taken just before the key). A difference is damage the client cannot see. This is the measure that gates W1.
2. **A fresh snapshot of the session** (`Rig::compare_to_snapshot`): a second scripted client attaches (it takes the session over, so this is the last thing a run does) and the terminal is compared with what the daemon draws from its own emulator. This is what a repaint would show, and what plan §5.6 `predict_resize` asserts.

They differ, and the plan's wording ("the screen ends equal to a fresh snapshot") is not the right test of the overlay: with **no** guesses at all the terminal sometimes ends different from a fresh snapshot (about 1 run in 5 on `cat`, `fill` and bash when narrowing, live or on QUIC). The daemon resizes its emulator at the point of the PTY's output where it reads the resize record, and the user's terminal resizes at the point of the output the client has shown, which is a round trip behind; the same reflow at two points of the stream leaves different screens. Over plain ssh the terminal is simply right, because it is the only screen. This skew is not the overlay's, and W1 does not have to repair it, though a repaint would. The control predictor (below) separates the two.

### The adapter

The harness reaches the predictor through one place, `Predict` and `Adapter`, so a change to the trait is one edit there. The variants:

| `Predict` | |
|---|---|
| `Off` | no predictor (`run_with(.., None)`) |
| `Passive` | the prediction machinery runs (shadow, overlay, the predictor called on every key and output) and the predictor never guesses. **The control** |
| `Mosh` | the M0 `MoshPredictor` with its own RTT threshold (20 ms), given the link's real RTT |
| `MoshAlways` | the same with the threshold at zero |
| `Eager` | a hostile predictor: a guess for every printable key, shown at once, no epochs |

The stock engine never gives the overlay an RTT (`Overlay::set_rtt` is not called anywhere), so the M0 predictor, whose threshold is 20 ms, would never predict. The adapter passes the link's configured RTT as `Ctx::rtt`. The adapter also keeps the predictor's side of the accounting (`AdapterStats`: guesses shown, confirmed, taken back, mispredicted), read from the edits it returned and the shadow screen it was given.

### What this part cannot do

It cannot classify each written glyph from the structured flush. The engine writes one buffer per flush (erase, server bytes and paint together, in synchronized-output brackets that the server's own bytes may also contain), so the paint cannot be told from the server's output from the bytes alone. The oracle terminal counts underlined glyphs instead, and the predictor's own accounting is the second count. The independent classifier needs the flush tap that T1 part 2 adds to `display.rs`.

It has no `--guard-off`. `Display` builds its overlay with `Config::default()` and the engine gives the harness no way to change that. The mutation is made another way, below.

## 2. Running it

```
just predict-e2e resize [--transport pipe|quic|both] [--seeds N] [--one-way-ms N] [--filter TEXT] [--verbose]
just predict-e2e baseline
just predict-e2e secret
cargo test -p fil-client --test predict_e2e                 # the cases that run in just ci
cargo test -p fil-client --test predict_e2e -- --ignored    # the resize reproduction
FIL_E2E_TRACE=1 ...                                         # prints every write (secret and status cases)
```

Real time, real PTYs, so everything takes seconds, not milliseconds. On Linux: `just vm-run cargo test -p fil-client --test predict_e2e` (`docs/dev/vm.md`).

| Run | macOS (Apple silicon) | Linux (the Lima VM, aarch64) |
|---|---|---|
| `cargo test -p fil-client --test predict_e2e` (8 tests, in `just ci`) | 7.4 s | passes (as part of the next row) |
| the same with `--include-ignored` (adds the reproduction, 6 seeds) | 21.4 s | 22.4 s, 9 of 9 pass |
| `predict_e2e secret` | 16.5 s | 16.2 s |
| `predict_e2e resize --seeds 5 --filter cat`, both transports | not timed on its own | 7 min |
| `predict_e2e baseline`, one transport, 4 seeds | 4 min with seven other runs in parallel | not run |
| the whole `resize` matrix, 72 cases of 5 runs | an estimated hour or more sequentially; under 30 min as six parallel processes, one per transport and program | not run |

Adding the file to `tests/common/mod.rs` costs every `fil-client` test binary one more module to compile; `cargo test -p fil-client` took 41 s in all on the development machine.

The Linux numbers on `cat` are the same as macOS's, case by case.

## 3. The result: resize in the true order damages the screen

**The reproduction succeeds.** W1's gate (plan §2.8) is met: with guesses on the screen, a resize in the true order leaves the user's terminal different from the client's shadow, and the same resize in the M0 order does not.

### The matrix

`just predict-e2e resize --seeds 5`, 75 ms each way (150 ms RTT), `MoshAlways`. A case is (transport, program, direction, order). Each runs five seeds, which set the typed text (14, 20, 26, 32 or 38 characters before the resize, 12 after), the timing of the keys, and nothing else. **live** is the real thing: output is in flight when the window changes. **frozen** holds the path from the last key until the client has answered, so no output arrives meanwhile; it isolates the overlay's erase and makes the M0 order exact. Cells are `damaged / runs` against the shadow, with `painted` the runs that had guesses on the screen at the resize; `control` is `Passive`.

| Program, direction | order | painted | **predict** | control |
|---|---|---|---|---|
| `cat`, 80 to 30 | true, live | 5 | **2 / 5** | 0 / 5 |
| | true, frozen | 5 | **2 / 5** | 0 / 5 |
| | M0, frozen | 5 | 0 / 5 | 0 / 5 |
| `cat`, 30 to 80 | true, live | 4 | 0 / 5 | 0 / 5 |
| | true, frozen | 4 | 0 / 5 | 0 / 5 |
| | M0, frozen | 4 | 0 / 5 | 0 / 5 |
| fill (8 long lines then `cat`), 80 to 30 | true, live | 5 | **2 / 5** | 0 / 5 |
| | true, frozen | 5 | **2 / 5** | 0 / 5 |
| | M0, frozen | 5 | 0 / 5 | 0 / 5 |
| fill, 30 to 80 | true, live | 4 | **4 / 5** | 0 / 5 |
| | true, frozen | 4 | **4 / 5** | 0 / 5 |
| | M0, frozen | 4 | 0 / 5 | 0 / 5 |
| bash (readline), 80 to 30 | true, live | 5 | **1 / 5** | 0 / 5 |
| | true, frozen | 5 | **2 / 5** | 0 / 5 |
| | M0, frozen | 5 | 0 / 5 | 0 / 5 |
| bash, 30 to 80 | true, live | 4 | 0 / 5 | 0 / 5 |
| | true, frozen | 4 | 0 / 5 | 0 / 5 |
| | M0, frozen | 4 | 0 / 5 | 0 / 5 |

That is the pipe. QUIC gives the same numbers in every row of predict bar one, and the same (clean) controls bar the same one: `bash`, 80 to 30, M0 order, frozen shows 1 of 5 runs different from the shadow in both the guessing run and the control run (rows 1 and 2). The QUIC freeze is a black hole with retransmission, not a held stream, so a late datagram can still be in the way; read that row as noise in the harness, not as damage.

Linux (the Lima VM) was run for `cat`, both transports, and matches macOS in every cell.

### What the numbers say

- **It is the overlay.** The control, which draws nothing, is clean against the shadow in every row (bar the QUIC row above), live and frozen. The guessing run is not.
- **It is the order.** The same case in the M0 order is clean in every row. That is why the M0 harness's `resize_with_predictions_showing` passed: it erased first, on the geometry the guesses had been drawn on.
- **Narrowing damages whenever the reflow moves a guess.** On `cat`, the damaged runs are seeds 3 and 4, the two whose typed text (32 and 38 characters) is longer than the new width of 30; seeds 1, 2 and 5 (20, 26 and 14 characters) are clean (checked by running seeds 1 to 2, 1 to 3 and 1 to 4). Damage needs a guess in a cell that the reflow moves.
- **Widening damages when the screen has wrapped lines to join.** `cat` and bash have none on the screen (the typed line fits), and are clean. The fill program leaves 8 long lines wrapped at 30; joining them moves everything below, and every run that had a guess on the screen is damaged.
- **Live and frozen agree**, so the output in flight at the resize is not what causes it, and the guessing run's damage is the same in both.

### What the damage looks like

`cat`, 80 to 30, 38 characters typed, true order (rows of the user's terminal against the shadow; `*` marks a row that differs):

```
  user's terminal                              | client's shadow
* wawetz i p  hymfof  amxxdjcjwj               | READY
*                           cjwj               | wawetz i p  hymfof  amxxdjcjwj
    gqpvuw qi or                               |   gqpvuw qi or
  cursors: (2, 14) | (2, 14)
```

The terminal re-wrapped the line, guesses included, so it needs one row more than the shadow's model of it; its screen scrolled by a row (`READY` is gone from the top); the client then erased the guesses at their old row and column, which left a blank hole and a **stale underlined guess** (`cjwj`) behind. Every later write lands one row off from where the shadow thinks. The difference was still there after 12 more keystrokes and their echoes. Nothing in the run repairs it: there is no snapshot, and the status line is not involved.

Widening, the fill program, true order: the terminal's cursor ends on **row 17 while the shadow's is on row 9**, and the typed line is split between the two rows (`rfytaignknantcm  q d` at row 9, the rest at 17). The program's next output goes to the wrong place.

The attributes differ too (`screen.attrs`, `screen.underline_style`: a guess left underlined), and so do the `wrapped` flags.

### The status line has it as well

The disconnect status line is drawn by the same overlay, so a resize under it is the same hazard (`DESIGN.md` §4.2 documents it as a known limit). `just predict-e2e resize --filter "fill narrow"` draws it (cut the path, wait for "fil: ..."), narrows the window to 30 columns, and compares the terminal with a fresh snapshot, leaving the status line's own rows (inverse video) out:

| | pipe | QUIC |
|---|---|---|
| true order, while disconnected | no difference outside the status rows | **row 1 differs** |
| M0 order, while disconnected | clean | clean |
| true order, after the path is back | clean (the resume repaired it, `want_snapshot`) | **rows 0 and 1 still differ** |
| M0 order, after the path is back | clean | differs in attributes only (no text) |

On the pipe the path coming back is a new link and a resume, and the resume's snapshot repairs the screen, as `DESIGN.md` §4.2 says. On QUIC the same outage is a `Degraded` link that **recovers by itself**: there is no resume and no snapshot, and the damage stays. That is the common case (a Wi-Fi hiccup), and §4.2's "until the next resume or swap" can mean never. The plan's rule 9 second bullet (repaint once the status line is removed) covers it; this reproduces the need. (One case each, not a matrix: it is the plan's second trigger and cheap to see, but the counts above are the evidence.)

### The decision this feeds

**W1 lands.** The reproduction is deterministic (every run whose reflow moves a guess), on both transports and both platforms, and the control and the M0-order run show that it is the erase running on a geometry the terminal has already left. T2's reflow finding is the second trigger and is not needed for this decision, but note what it cannot change: the oracle here is the same emulator as the shadow (alacritty), so this reproduces the overlay's order error, not a real terminal reflowing unlike the shadow.

What W1's tests should assert, using this harness:

- `a_narrowing_resize_with_guesses_on_screen_damages_it_in_the_true_order` (`#[ignore]`d, in `predict_e2e.rs`) passes today and must be flipped to assert the opposite when W1 lands (its six seeds then end with `compare_to_shadow` clean, or the terminal equal to a fresh snapshot once the repair has landed);
- add the widening fill case, which needs W1's repaint, not just a no-write rule: the terminal's cursor is on the wrong row;
- `the_resize_controls_are_clean` stays as it is.

## 4. The baseline, with `EchoAck` off

`just predict-e2e baseline`: faithful typing (a 41-character command, 4 seeds, 10 to 90 ms between keys) into `cat` and an interactive bash, the M0 predictor, both transports. `EchoAck` is off because the stock client asks for none. "Always" is the threshold at zero; "adaptive" is the M0 predictor's 20 ms threshold with the link's real RTT. `shown` is the predictor's own count of guesses it showed; `paints` is the oracle terminal's count of underlined glyphs written (spaces do not count: an underlined blank has no glyph, which is the 28 between `shown` and `paints`); `flashes` is written guesses erased without being confirmed.

| RTT | predictor | keys | paints | flashes | shown | confirmed | mispredicted | screen at the end |
|---|---|---|---|---|---|---|---|---|
| 10 ms | always | 164 | 132 | 0 | 160 | 160 | 0 | = snapshot |
| 10 ms | adaptive | 164 | 0 | 0 | 0 | 0 | 0 | = snapshot |
| 50, 150, 400 ms | always | 164 | 132 | 0 | 160 | 160 | 0 | = snapshot |
| 50, 150, 400 ms | adaptive | 164 | 132 | 0 | 160 | 160 | 0 | = snapshot |

The same for `cat` and bash, pipe and QUIC (32 rows; the 50 ms, 150 ms and 400 ms rows are identical, so they are merged). **Zero visible mispredictions on faithful echo, and nothing painted at 10 ms with the adaptive threshold.** That is the bar P1 has to keep, and it is the number to compare with once `EchoAck` is on. The M0 predictor is not tested against the cases that make it hard (autosuggestion, `RPROMPT`, a slow echo, mid-line edits): those are P1's and part 2's suites.

## 5. Secrets, and the full-stack mutation

`just predict-e2e secret`: `read -s` (a prompt, then a second later `stty -echo` and a read), with a secret of reserved-alphabet characters typed by the seeded typist. *Ahead* means typing starts the moment the program turns echo off, before the client can have heard (the typist watches a marker file the program touches). *After* waits for the daemon's second `PtyMode` to arrive.

| Case | pipe leaked | QUIC leaked |
|---|---|---|
| M0 predictor, typed ahead of the report | 0 | 0 |
| M0 predictor, typed after the report | 0 | 0 |
| hostile predictor, typed after the report | **0** | **0** |
| **MUTATION**: hostile predictor, `PtyMode` withheld from the client | **16 of 16** | **16 of 16** |
| hostile predictor, typed ahead of the report | 3 to 6 | 3 to 5 |

- The core's password rule works through the whole stack: a predictor that shows every key at once is stopped once the report has arrived.
- The mutation is the harness's check that it can see a leak. The plan asks for `--guard-off password` to fail on `read -s`; the engine gives the harness no way to switch a guard off (`Display::overlay_for` uses `Config::default()`), so the daemon's `PtyMode` is withheld from the client instead (`Faults::drop_pty_mode`, a wrapper link that drops them), which takes away what the guard needs. The leak is seen: all 16 reserved characters reach the terminal. Both this and the "after the report" case are tests in `just ci`.
- The last row is what the M0 predictor's epochs are for, and what C1's core must do for **any** predictor: keys typed in the window between the PTY turning echo off and the client learning of it are written by a predictor that does not keep its own epochs. That window is one daemon poll plus one way trip; here it held 3 to 6 of the 16 characters (3 and 3 on macOS, 6 and 5 on Linux). The row is reported and not asserted: it is C1's `sudo`-typed-ahead trace, run on the whole stack.

## 6. For C1: what the adapter needs

Everything that names the predictor API is in `oracle_term.rs`, in the "adapter" section. If `fil-predict` changes, these are the edits:

- `impl Predictor for Adapter`, `Passive`, `EagerPredictor`: the trait's methods (`on_key`, `on_output`, `on_tick`, `next_deadline`, `reset`, `has_outstanding`, `wants_alt_screen`) and their arguments (`KeyEvent`, `Ctx { screen, rtt, now }`). A new required method, a new `Ctx` field or a changed return type is one edit per impl. `Adapter` forwards to an inner `Box<dyn Predictor>` and records `ctx.screen` (a `TermState`) on every call: if the shadow is handed over as a `ScreenView` and not a `TermState`, `Adapter::shadow` and `compare_to_shadow` need it converted (they use only `screen`, `cursor` and `modes` through `fil_predict::oracle::diffs`).
- `Adapter::apply` reads the edits (`OverlayEdit::{Put, Remove, Clear, Cursor}`) to know what the predictor shows. If edits become *declared guesses* (shown or not, tagged with the input offset), count `shown` from the ones the core writes, or keep counting from the declared and show-able ones as now and say so in the table.
- `predictor_factory`: how the M0 predictor is built (`MoshPredictor::new(MoshConfig { rtt_threshold, .. })`) and what the factory returns (`PredictorFactory`, from `fil-client`). With P1's adaptive switch the RTT threshold and the injected RTT go away: the adapter then passes the link hint through whatever `LinkView` becomes, or nothing.
- `fil_predict::sim::Rng` (the seeded typist) and `fil_predict::oracle::diffs` / `Capabilities` (the comparison) are used as public functions: keep them public or tell this file's owner.
- `CellStyle::Underline` is how the oracle terminal recognizes a written guess. If P1 stops underlining (plan §2.7 underlines only above 80 ms or in a glitch window), the `paints` and `flashes` counts go to zero at low RTT by design; use the adapter's `shown`, or have the harness count the style P1 uses. The C1 tests of the password rule and the resize damage do not depend on it for `Eager` and `MoshAlways` at 150 ms.
- The tests that must still hold after C1: `the_resize_controls_are_clean`, the three secret tests, the two typing tests. The `#[ignore]`d reproduction should *keep* passing until W1.

## 7. Findings about the plan

1. **The resize oracle.** §3 T1 and §5.6 say the terminal must end "equal to a fresh snapshot". That test fails for reasons that are not the overlay (a daemon and a terminal that reflow at two points of the stream, §1 above): 1 run in 5 on `cat` with no guesses. The right oracle for the overlay is the client's shadow; the snapshot comparison is right once W1's repaint has run. V1's `predict_resize` should say which, or it will be flaky.
2. **`--guard-off` through the full stack needs a seam.** The plan's T1 mutation ("`--guard-off password` must fail on `read -s`") cannot be made without changing `Display::overlay_for` or `ClientConfig` (E1a's or part 2's files). Proposed: the test feature that part 2's tap uses also lets a test pass the overlay's `Config`. Until then the withheld `PtyMode` is the mutation (§5).
3. **The stock engine passes no RTT.** `Overlay::set_rtt` has no caller, so the M0 predictor (threshold 20 ms) never predicts under the real engine. E1a's `LinkView` fixes it; the harness injects the RTT meanwhile.
4. **No test feature was added in part 1.** The plan lists "a test feature in `Cargo.toml`"; nothing in part 1 needs one (the tap is part 2's, and the example and test use dev-dependencies), so it comes with part 2.
5. **The engine has no `on_tick`.** `Display` never calls `Overlay::on_tick`, so the predictor's timeout (1.5 s) never fires and a guess that is never echoed stays on the screen until output comes. The 10 s backstop of §2.9 needs a tick source in E1a.
6. **Widening damages too, and in a way a "no write until repaint" rule alone does not repair**: the terminal's cursor ends on a different row. Rule 9's "no guess after a resize until a snapshot at the current generation" stops the *next* damage; the repaint is what repairs the screen that is already wrong. W1 has to ask for it when guesses were on the screen at the resize, not only when the next one is about to be written.
