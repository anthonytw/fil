"""Turns what a terminal's read-back command prints (text with SGR escapes) into rows of cells.

The read-back formats differ: tmux, kitty and WezTerm print the text with SGR sequences, xterm's
print-screen adds `ESC # 5` and a form feed. All of them are reduced to the same thing here: a
list of rows, each a list of cells `(text, width, flags)`, where `flags` is the sorted tuple of
the attribute tokens the exporter uses (`b d i u r s k`, `f<color>`, `g<color>`).
"""

import re
import unicodedata

_CSI = re.compile(r"\x1b\[([0-9:;<=>?]*)([ -/]*)([@-~])")
_OSC = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")


def cell_width(ch):
    """Display width of one code point as a terminal that follows East Asian Width draws it."""
    if unicodedata.combining(ch) or ch in "​‌‍︎️⃣" or "\U0001f3fb" <= ch <= "\U0001f3ff":
        return 0
    if unicodedata.category(ch) in ("Mn", "Me", "Cf"):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1


class Pen:
    def __init__(self):
        self.reset()

    def reset(self):
        self.bold = self.dim = self.italic = self.under = False
        self.inverse = self.strike = self.blink = False
        self.fg = self.bg = None

    def apply(self, param_text):
        # Colon-separated sub-parameters (`4:3`, `38:2::1:2:3`) are flattened: for 4 the style
        # follows (4:0 means no underline); for 38/48/58 a `2` form with a colorspace id has
        # six values, the semicolon form five.
        raw = param_text
        groups = raw.split(";") if raw else ["0"]
        flat = []
        for g in groups:
            subs = g.split(":")
            nums = [int(s) if s.isdigit() else 0 for s in subs]
            if len(nums) > 1 and nums[0] in (38, 48, 58) and nums[1] == 2 and len(nums) >= 6:
                nums = [nums[0], 2, nums[3], nums[4], nums[5]]
            elif len(nums) > 1 and nums[0] == 4:
                flat.extend([("u", nums[1])])
                continue
            flat.extend(nums)
        i = 0
        while i < len(flat):
            p = flat[i]
            if isinstance(p, tuple):
                self.under = p[1] != 0
                i += 1
                continue
            if p == 0:
                self.reset()
            elif p == 1:
                self.bold = True
            elif p == 2:
                self.dim = True
            elif p == 3:
                self.italic = True
            elif p in (4, 21):
                self.under = True
            elif p in (5, 6):
                self.blink = True
            elif p == 7:
                self.inverse = True
            elif p == 9:
                self.strike = True
            elif p == 22:
                self.bold = self.dim = False
            elif p == 23:
                self.italic = False
            elif p == 24:
                self.under = False
            elif p == 25:
                self.blink = False
            elif p == 27:
                self.inverse = False
            elif p == 29:
                self.strike = False
            elif 30 <= p <= 37:
                self.fg = str(p - 30)
            elif p == 39:
                self.fg = None
            elif 40 <= p <= 47:
                self.bg = str(p - 40)
            elif p == 49:
                self.bg = None
            elif 90 <= p <= 97:
                self.fg = str(p - 90 + 8)
            elif 100 <= p <= 107:
                self.bg = str(p - 100 + 8)
            elif p in (38, 48):
                tok, j = self._color(flat, i)
                if tok is not None:
                    if p == 38:
                        self.fg = tok
                    else:
                        self.bg = tok
                i = j
                continue
            elif p == 58:
                _, j = self._color(flat, i)
                i = j
                continue
            i += 1

    @staticmethod
    def _color(flat, i):
        if i + 2 < len(flat) and flat[i + 1] == 5:
            return str(flat[i + 2]), i + 3
        if i + 4 < len(flat) and flat[i + 1] == 2:
            r, g, b = flat[i + 2], flat[i + 3], flat[i + 4]
            return "#%02x%02x%02x" % (r & 255, g & 255, b & 255), i + 5
        return None, i + 1

    def flags(self):
        v = []
        if self.bold:
            v.append("b")
        if self.dim:
            v.append("d")
        if self.italic:
            v.append("i")
        if self.under:
            v.append("u")
        if self.inverse:
            v.append("r")
        if self.strike:
            v.append("s")
        if self.blink:
            v.append("k")
        if self.fg is not None:
            v.append("f" + self.fg)
        if self.bg is not None:
            v.append("g" + self.bg)
        return tuple(sorted(v))


def parse(text, cr_wraps=False):
    """Rows of cells `(text, width, flags)`. Lines end at `\\n`; form feeds and the escape
    sequences that do not matter here are dropped. With `cr_wraps` a carriage return that no
    line feed follows ends a row too: kitty's `--add-wrap-markers` puts one at each soft wrap
    (it joins soft-wrapped lines otherwise)."""
    rows = [[]]
    pen = Pen()
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == "\x1b":
            m = _CSI.match(text, i)
            if m:
                if m.group(3) == "m" and not m.group(2):
                    pen.apply(m.group(1))
                i = m.end()
                continue
            m = _OSC.match(text, i)
            if m:
                i = m.end()
                continue
            # ESC ( B, ESC # 5, ESC =, ...: two or three bytes.
            if i + 1 < n and text[i + 1] in "()*+#%":
                i += 3
            else:
                i += 2
            continue
        if ch == "\n":
            rows.append([])
        elif ch == "\r":
            if cr_wraps and text[i + 1 : i + 2] != "\n":
                rows.append([])
        elif ch == "\f":
            pass
        elif ch == "\x0f" or ch == "\x0e":
            pass
        else:
            w = cell_width(ch)
            if w == 0:
                if rows[-1]:
                    t, ww, f = rows[-1][-1]
                    rows[-1][-1] = (t + ch, ww, f)
                    # A VS16 widens a text-presentation character in terminals that honor it;
                    # the width reported stays what the terminal's own cell count says
                    # (measured with the cursor), so nothing is changed here.
            else:
                rows[-1].append((ch, w, pen.flags()))
        i += 1
    return rows


def row_text(row):
    return "".join(c[0] for c in row).rstrip()


def attr_cells(rows, keys):
    """`{(row, col): flags-string}` for the cells that carry any flag in `keys`, leaving out the
    blank cells after the last character of a row (terminals trim those)."""
    out = {}
    for r, row in enumerate(rows):
        last = 0
        col = 0
        for t, w, _ in row:
            col += w
            if t.strip():
                last = col
        col = 0
        for t, w, f in row:
            if col >= last:
                break
            sel = tuple(x for x in f if x[0] in keys)
            if sel:
                out[(r, col)] = ",".join(sorted(sel))
            col += w
    return out
