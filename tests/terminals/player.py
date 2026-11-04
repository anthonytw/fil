#!/usr/bin/env python3
"""Runs inside the terminal under test: replays one trace into it and reads the screen back.

    player.py --term xterm|kitty|wezterm|tmux --trace FILE --out RESULT.jsonl [--dump DIR]

The terminal started this program as its child, so the program's standard input and output are
the terminal's pty. For each step of the trace:

  write   the bytes are written to the terminal;
  resize  the terminal is resized by the means the terminal offers (and the pty's window size
          is waited for);
  check   a cursor position report (CSI 6 n) is sent as a barrier: the terminal answers only
          after it has parsed everything written before it, and the answer is the cursor to
          compare. Then the screen (and the scrollback, where the terminal can give it) is read
          back by the terminal's own command and compared with the trace's expectation, which
          came from the shadow model (tests/terminals/exporter).

One JSON object per line goes to RESULT.jsonl, a last `{"done": true}` after the final step.
A step that cannot be carried out (no answer to the barrier, a read-back command that fails)
is recorded as an `error`, which fails the suite: a terminal that cannot be read must not
pass by showing nothing.
"""

import argparse
import base64
import fcntl
import json
import os
import re
import select
import struct
import subprocess
import sys
import termios
import time
import tty

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ansi  # noqa: E402

CPR = re.compile(rb"\x1b\[(\d+);(\d+)R")


class ReadbackError(Exception):
    pass


def out(b):
    while b:
        n = os.write(1, b)
        b = b[n:]


def winsize():
    r = fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8)
    rows, cols, _, _ = struct.unpack("HHHH", r)
    return cols, rows


def cpr(timeout=15.0):
    """Asks for the cursor position and returns (row, col), 1-based."""
    termios.tcflush(0, termios.TCIFLUSH)
    out(b"\x1b[6n")
    buf = b""
    end = time.time() + timeout
    while time.time() < end:
        r, _, _ = select.select([0], [], [], 0.1)
        if r:
            buf += os.read(0, 4096)
            m = CPR.search(buf)
            if m:
                return int(m.group(1)), int(m.group(2))
    raise ReadbackError("no answer to the cursor position report (barrier)")


def run(cmd, timeout=15, env=None):
    try:
        r = subprocess.run(cmd, capture_output=True, timeout=timeout, env=env)
    except (subprocess.TimeoutExpired, OSError) as e:
        raise ReadbackError(f"{cmd[0]}: {e}") from e
    if r.returncode != 0:
        raise ReadbackError(f"{' '.join(cmd)}: exit {r.returncode}: {r.stderr.decode(errors='replace')[:200]}")
    return r.stdout.decode("utf-8", errors="replace")


# ---- the terminals ---------------------------------------------------------------------------


class Term:
    name = ""
    # Whether the scrollback can be read back.
    history = False

    def resize(self, cols, rows):
        raise NotImplementedError

    def screen(self):
        """The visible rows as raw text with SGR escapes, one row per line."""
        raise NotImplementedError

    def scrollback(self, screen_rows):
        """The scrollback as raw text, oldest first, or None if the terminal cannot give it."""
        return None


class Tmux(Term):
    name = "tmux"
    history = True

    def __init__(self):
        self.pane = os.environ["TMUX_PANE"]
        self.base = ["tmux", "-L", os.environ["FIL_TMUX_SOCKET"]]

    def resize(self, cols, rows):
        run(self.base + ["resize-window", "-t", self.pane, "-x", str(cols), "-y", str(rows)])

    def screen(self):
        return run(self.base + ["capture-pane", "-p", "-e", "-t", self.pane])

    def scrollback(self, screen_rows):
        n = int(run(self.base + ["display-message", "-p", "-t", self.pane, "#{history_size}"]).strip())
        if n == 0:
            return ""
        return run(self.base + ["capture-pane", "-p", "-e", "-t", self.pane, "-S", str(-n), "-E", "-1"])


class Kitty(Term):
    name = "kitty"
    history = True

    def resize(self, cols, rows):
        run(["kitty", "@", "resize-os-window", "--self", "--unit", "cells", "--width", str(cols), "--height", str(rows)])

    def screen(self):
        return run(["kitty", "@", "get-text", "--ansi", "--add-wrap-markers", "--extent", "screen"])

    def scrollback(self, screen_rows):
        # `all` is the scrollback followed by the screen; both drop the screen's trailing blank
        # rows the same way, so the scrollback is what comes before the screen's own lines.
        allt = run(["kitty", "@", "get-text", "--ansi", "--add-wrap-markers", "--extent", "all"])
        n_all = len(history_rows(allt))
        n_scr = len(history_rows(self.screen()))
        return take_rows(allt, max(0, n_all - n_scr))


def take_rows(text, k):
    """The first k rows of kitty's text: a row ends at a newline (with its carriage return) or at
    the lone carriage return that marks a soft wrap."""
    pos = 0
    for _ in range(k):
        m = re.compile(r"\r?\n|\r").search(text, pos)
        if not m:
            break
        pos = m.end()
    return text[:pos]


class Wezterm(Term):
    name = "wezterm"
    history = True

    def __init__(self):
        self.pid = os.environ.get("WEZTERM_UNIX_SOCKET", "").rsplit("-", 1)[-1]

    def _cell(self):
        j = json.loads(run(["wezterm", "cli", "list", "--format", "json"]))
        p = j[0]["size"]
        return p["pixel_width"] // p["cols"], p["pixel_height"] // p["rows"]

    def resize(self, cols, rows):
        cw, ch = self._cell()
        ids = run(["xdotool", "search", "--pid", self.pid]).split() if self.pid.isdigit() else []
        if not ids:
            ids = run(["xdotool", "search", "--class", "org.wezfurlong.wezterm"]).split()
        for wid in ids:
            run(["xdotool", "windowsize", wid, str(cols * cw), str(rows * ch)])

    def screen(self):
        # Without a range the text starts above the viewport after the screen has scrolled.
        rows = winsize()[1]
        return run(["wezterm", "cli", "get-text", "--escapes", "--start-line", "0", "--end-line", str(rows - 1)])

    def _dims(self):
        path = os.path.join(os.environ.get("TMPDIR", "/tmp"), f"fil-wez-dims-{os.getpid()}")
        for _ in range(5):
            if os.path.exists(path):
                os.unlink(path)
            out(b"\x1b]1337;SetUserVar=fil_dims=" + base64.b64encode(path.encode()) + b"\x07")
            end = time.time() + 3
            while time.time() < end:
                if os.path.exists(path):
                    time.sleep(0.02)
                    with open(path) as f:
                        vals = [int(x) for x in f.read().split()]
                    os.unlink(path)
                    return vals
                time.sleep(0.02)
        raise ReadbackError("WezTerm did not report the pane's dimensions")

    def scrollback(self, screen_rows):
        # Lines above the screen have negative numbers.
        total, view, top, first = self._dims()
        self.dims = (total, view, top, first)
        self.hist_n = 0
        # Lines above the viewport: the physical top row less the first row of the scrollback.
        n = top - first
        if n <= 0:
            return ""
        self.hist_n = n
        return run(["wezterm", "cli", "get-text", "--escapes", "--start-line", str(-n), "--end-line", "-1"])


class Xterm(Term):
    name = "xterm"
    history = True

    def __init__(self):
        self.file = os.environ["FIL_XTERM_PRINT"]
        self.dir = os.environ["FIL_XTERM_IMMEDIATE"]
        self.pos = 0
        self.focused = False

    def scrollback(self, screen_rows):
        # The print-immediate action (key F5) writes the saved lines and the screen to a file;
        # the screen is its last rows.
        if not self.focused:
            wid = run(["xdotool", "search", "--pid", str(os.getppid()), "--class", "xterm"]).split()
            if not wid:
                wid = run(["xdotool", "search", "--class", "xterm"]).split()
            self.wid = wid
            run(["xdotool", "windowfocus", wid[0]])
            self.focused = True
        for f in os.listdir(self.dir):
            os.unlink(os.path.join(self.dir, f))
        run(["xdotool", "key", "F5"])
        end = time.time() + 15
        while time.time() < end:
            for f in os.listdir(self.dir):
                path = os.path.join(self.dir, f)
                # The file has no end marker: it is complete when it stops growing.
                data, last = b"", -1
                while len(data) != last:
                    last = len(data)
                    time.sleep(0.1)
                    with open(path, "rb") as fh:
                        data = fh.read()
                if data:
                    os.unlink(path)
                    rows = ansi.parse(data.decode("utf-8", errors="replace"))
                    if rows and not rows[-1]:
                        rows.pop()  # the last line's newline
                    keep = rows[: max(0, len(rows) - screen_rows)]
                    return "\n".join(ansi.row_text(r) for r in keep) + ("\n" if keep else "")
            time.sleep(0.02)
        raise ReadbackError(f"xterm wrote no file for print-immediate in {self.dir}: {os.listdir(self.dir)}")

    def resize(self, cols, rows):
        out(b"\x1b[8;%d;%dt" % (rows, cols))

    def screen(self):
        # CSI i: print the screen. xterm's printerCommand appends it to a file, ending with a
        # form feed.
        out(b"\x1b[0i")
        end = time.time() + 15
        while time.time() < end:
            try:
                with open(self.file, "rb") as f:
                    f.seek(self.pos)
                    data = f.read()
            except FileNotFoundError:
                data = b""
            if data.endswith(b"\f"):
                self.pos += len(data)
                return data.decode("utf-8", errors="replace")
            time.sleep(0.02)
        raise ReadbackError("xterm printed nothing for CSI i")


def make_term(name):
    return {"tmux": Tmux, "kitty": Kitty, "wezterm": Wezterm, "xterm": Xterm}[name]()


# ---- comparing ---------------------------------------------------------------------------------


# Whether the terminal marks soft wraps with a lone carriage return in its read-back (kitty).
CR_WRAPS = False
# Whether the read-back is xterm's print-screen.
XTERM_PRINT = False


def rows_of(raw, nrows, pad=True):
    rows = ansi.parse(raw, CR_WRAPS)
    # A trailing newline ends the last row; xterm's form feed leaves one empty row after it.
    while len(rows) > nrows and not rows[-1]:
        rows.pop()
    if pad:
        while len(rows) < nrows:
            rows.append([])
    return rows


def history_rows(raw):
    """The scrollback's rows; a text that ends in a newline does not have an extra empty row."""
    if raw is None:
        return None
    rows = ansi.parse(raw, CR_WRAPS)
    if rows and not rows[-1]:
        rows.pop()
    return rows


def brief(s, n=70):
    s = repr(s)
    return s if len(s) <= n else s[: n - 3] + "...'"


def compare(exp, attrs_keys, screen_rows, hist_rows, cur, observed):
    """The differences between the expectation and what was read, as short strings."""
    diffs = []
    if "screen" in exp:
        want = exp["screen"]
        sel = exp.get("rows")
        for i, w in enumerate(want):
            if sel is not None and i not in sel:
                continue
            g = ansi.row_text(screen_rows[i]) if i < len(screen_rows) else ""
            if g != w:
                diffs.append(f"row {i}: want {brief(w)} got {brief(g)}")
    # kitty's cursor position report is wrong inside origin mode (it subtracts the region's top
    # twice, as far as it was measured), so the cursor is not compared there; the probe shows
    # where the cursor really is.
    if "cursor" in exp and not (CR_WRAPS and "cursor_absolute" in exp) and list(cur) != exp["cursor"] and not (
        exp.get("pending_wrap") and cur[0] == exp["cursor"][0] and cur[1] == exp["cursor"][1] + 1
    ) and list(cur) != exp.get("cursor_absolute") and not (
        exp.get("pending_wrap") and cur[1] == 1 and cur[0] == exp["cursor"][0] + 1
    ):
        diffs.append(f"cursor (row, col) want {tuple(exp['cursor'])} got {tuple(cur)}")
    if "attrs" in exp:
        want = {(r, c): f for r, c, f in exp["attrs"]}
        got = ansi.attr_cells(screen_rows, attrs_keys)
        if XTERM_PRINT:
            # xterm's print-screen leaves the pen as it was when the next cell's attributes go
            # back to the default, and omits it across blanks: a cell right after one with
            # attributes seems to have the same ones (a program that writes `ESC [ 7 m  one `
            # over `line 21` reads back with `21` in inverse). So blank cells are left out, and
            # so is a cell that has exactly the flags of the cell before it unless the model
            # expects them. An unexpected attribute that spreads to the next cell is therefore
            # not seen on xterm.
            drop = set()
            for r, row in enumerate(screen_rows):
                col, prev = 0, None
                for t, w, _ in row:
                    f = got.get((r, col))
                    if not t.strip():
                        drop.add((r, col))
                    elif f is not None and f == prev and (r, col) not in want:
                        drop.add((r, col))
                    prev = f
                    col += w
            got = {k: v for k, v in got.items() if k not in drop}
            want = {k: v for k, v in want.items() if k not in drop}
        bad = []
        for k in sorted(set(want) | set(got)):
            if want.get(k) != got.get(k):
                bad.append(f"({k[0]},{k[1]}) want {want.get(k, '-')} got {got.get(k, '-')}")
        if bad:
            diffs.append(f"attrs: {len(bad)} cells differ: " + "; ".join(bad[:4]) + (" ..." if len(bad) > 4 else ""))
    if "history_count" in exp:
        if hist_rows is None:
            observed["history"] = "unobservable"
        else:
            observed["history_count"] = len(hist_rows)
            if len(hist_rows) != exp["history_count"]:
                diffs.append(f"scrollback lines: want {exp['history_count']} got {len(hist_rows)}")
            elif "history" in exp:
                for i, w in enumerate(exp["history"]):
                    g = ansi.row_text(hist_rows[i])
                    if g != w:
                        diffs.append(f"scrollback row {i}: want {brief(w)} got {brief(g)}")
                        break
    if "content" in exp:
        if hist_rows is None and exp.get("history_count", 0) > 0:
            observed["content"] = "unobservable (scrollback)"
        else:
            got = "".join(
                "".join(c[0] for c in r) for r in (hist_rows or []) + screen_rows
            )
            got = "".join(ch for ch in got if not ch.isspace())
            if got != exp["content"]:
                k = next((i for i, (a, b) in enumerate(zip(got, exp["content"])) if a != b), min(len(got), len(exp["content"])))
                diffs.append(f"content: {len(exp['content'])} characters expected, {len(got)} read; first difference at {k}")
    return diffs


# ---- the replay --------------------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--term", required=True)
    ap.add_argument("--trace", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--dump", default="")
    args = ap.parse_args()

    with open(args.trace) as f:
        trace = json.load(f)
    name = trace["name"]
    term = make_term(args.term)
    global CR_WRAPS, XTERM_PRINT
    CR_WRAPS = args.term == "kitty"
    XTERM_PRINT = args.term == "xterm"
    res = open(args.out, "a", buffering=1)

    def emit(obj):
        obj["trace"] = name
        res.write(json.dumps(obj) + "\n")

    tty.setraw(0)
    try:
        cols, rows = winsize()
        if (cols, rows) != (trace["cols"], trace["rows"]):
            emit({"error": f"the terminal started at {cols}x{rows}, not {trace['cols']}x{trace['rows']}"})
        # The terminal starts empty; make sure the cursor is home with a clean pen.
        out(b"\x1b[0m\x1b[H")
        cpr()
        for step in trace["steps"]:
            op = step["op"]
            if op == "write":
                out(bytes.fromhex(step["hex"]))
            elif op == "resize":
                term.resize(step["cols"], step["rows"])
                end = time.time() + 15
                while time.time() < end and winsize() != (step["cols"], step["rows"]):
                    time.sleep(0.05)
                cols, rows = winsize()
                if (cols, rows) != (step["cols"], step["rows"]):
                    raise ReadbackError(f"resize to {step['cols']}x{step['rows']} left the window {cols}x{rows}")
                cpr()
            elif op == "check":
                cols, rows = winsize()
                observed = {}
                try:
                    try:
                        cur = cpr(2.0 if step.get("optional_barrier") else 15.0)
                    except ReadbackError:
                        if not step.get("optional_barrier"):
                            raise
                        emit({"check": step["name"], "status": "info", "observed": {},
                              "diffs": ["no cursor position report: the terminal holds its replies inside a synchronized-output bracket"],
                              "refs": step.get("refs"), "note": step.get("note", "")})
                        continue
                    raw = term.screen()
                    screen_rows = rows_of(raw, rows)
                    hist_raw = term.scrollback(rows) if "history_count" in step["expect"] else None
                    hist_rows = history_rows(hist_raw)
                    if hist_rows is not None and getattr(term, "hist_n", None) is not None:
                        # WezTerm ends the text with an extra SGR and newline: an empty row.
                        hist_rows = hist_rows[: term.hist_n]
                    observed["cursor"] = list(cur)
                    if getattr(term, "dims", None):
                        observed["dims"] = list(term.dims)
                    diffs = compare(step["expect"], step.get("attrs", ""), screen_rows, hist_rows, cur, observed)
                except ReadbackError as e:
                    emit({"check": step["name"], "error": str(e)})
                    if args.dump:
                        pass
                    break
                status = "ok" if not diffs else ("info" if step.get("info") else "diff")
                emit({
                    "check": step["name"], "status": status, "diffs": diffs,
                    "observed": observed, "refs": step.get("refs"), "note": step.get("note", ""),
                    "expected": {k: step["expect"].get(k) for k in ("cursor", "history_count")},
                })
                if args.dump:
                    with open(os.path.join(args.dump, f"{name}.{step['name'].replace('/', '_')}.txt"), "w") as d:
                        d.write(raw)
                        if hist_raw is not None:
                            d.write("\n---- scrollback ----\n" + hist_raw)
        else:
            emit({"done": True})
    except ReadbackError as e:
        emit({"error": str(e)})
    except Exception as e:  # noqa: BLE001 - recorded, and the run fails on it
        emit({"error": f"{type(e).__name__}: {e}"})
    finally:
        out(b"\x1b[0m\x1b[?25h")


if __name__ == "__main__":
    main()
