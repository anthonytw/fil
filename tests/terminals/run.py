#!/usr/bin/env python3
"""Runs the traces in a directory through one terminal and reports.

    run.py TERMINAL TRACE_DIR OUT_DIR [--only SUBSTRING]... [--timeout SECONDS]

TERMINAL is xterm, kitty, wezterm or tmux. Each trace gets a fresh terminal of the trace's size,
started with `player.py` as its program; the results land in OUT_DIR/TERMINAL.jsonl. Needs an X
display (Xvfb) for every terminal but tmux; tests/terminals/run.sh provides one.

Exit status: 0 if every check agreed or is a known disagreement (known.tsv); 1 if there is a new
disagreement, a check that could not be read, or a trace that did not finish.
"""

import argparse
import hashlib
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PLAYER = os.path.join(HERE, "player.py")


TMUX_N = 0
# A short private directory for sockets and small files (a unix socket path is limited to about
# 100 bytes; nothing predictable in a shared /tmp).
TMP = ""


def launch(term, trace, work, result, dump):
    """Starts the terminal with the player in it; returns (Popen, cleanup)."""
    py = [sys.executable, "-I", PLAYER, "--term", term, "--trace", trace["path"], "--out", result]
    if dump:
        py += ["--dump", dump]
    cols, rows = trace["cols"], trace["rows"]
    env = dict(os.environ, LANG="C.UTF-8", LC_ALL="C.UTF-8")
    env.pop("TMUX", None)
    env["TMPDIR"] = TMP
    if term == "xterm":
        printed = os.path.join(work, "print.out")
        open(printed, "w").close()
        env["FIL_XTERM_PRINT"] = printed
        # print-immediate (bound to F5, sent with xdotool) writes the screen and the saved lines
        # to a timestamped file in this directory; CSI i (printerCommand) gives the screen alone.
        immediate = os.path.join(work, "immediate")
        os.makedirs(immediate, exist_ok=True)
        env["FIL_XTERM_IMMEDIATE"] = immediate
        cmd = [
            "xterm", "-geometry", f"{cols}x{rows}", "-u8",
            "-xrm", "XTerm*allowWindowOps: true",
            "-xrm", "XTerm*saveLines: 2000",
            "-xrm", f"XTerm*printerCommand: cat >> {shlex.quote(printed)}",
            "-xrm", "XTerm*printAttributes: 2",
            "-xrm", "XTerm*printerFormFeed: true",
            "-xrm", "XTerm*printerExtent: true",
            "-xrm", f"XTerm*printFileImmediate: {immediate}/p",
            "-xrm", "XTerm*printModeImmediate: 2",
            "-xrm", "XTerm*printOptsImmediate: 0",
            "-xrm", "XTerm*VT100.translations: #override <Key>F5: print-immediate()",
            "-e", *py,
        ]
        return subprocess.Popen(cmd, env=env, start_new_session=True, stderr=open(os.path.join(work, "stderr.log"), "w"))
    if term == "kitty":
        env["LIBGL_ALWAYS_SOFTWARE"] = "1"
        # A unix socket path is limited to about 100 bytes; the work directory can be longer.
        sock = os.path.join(TMP, "k.sock")
        cmd = [
            "kitty", "--config", "NONE",
            "-o", "allow_remote_control=socket-only",
            "-o", f"listen_on=unix:{sock}",
            "-o", f"initial_window_width={cols}c", "-o", f"initial_window_height={rows}c",
            "-o", "remember_window_size=no", "-o", "scrollback_lines=2000",
            "-o", "confirm_os_window_close=0", "-o", "enable_audio_bell=no",
            "-o", "hide_window_decorations=yes", "-o", "update_check_interval=0",
            *py,
        ]
        env["KITTY_LISTEN_ON"] = f"unix:{sock}"
        return subprocess.Popen(cmd, env=env, start_new_session=True, stderr=open(os.path.join(work, "stderr.log"), "w"))
    if term == "wezterm":
        cfg = os.path.join(work, "wezterm.lua")
        with open(cfg, "w") as f:
            f.write(
                # The scrollback's size is not in `wezterm cli`; a user variable set by the
                # player (OSC 1337 SetUserVar) makes this handler write the pane's dimensions.
                "local wezterm = require 'wezterm'\n"
                "wezterm.on('user-var-changed', function(window, pane, name, value)\n"
                "  if name == 'fil_dims' then\n"
                "    local d = pane:get_dimensions()\n"
                "    local f = io.open(value, 'w')\n"
                "    f:write(string.format('%d %d %d %d\\n', d.scrollback_rows, d.viewport_rows, d.physical_top, d.scrollback_top))\n"
                "    f:close()\n"
                "  end\n"
                "end)\n"
                "return {\n"
                f"  initial_cols = {cols}, initial_rows = {rows},\n"
                "  scrollback_lines = 2000, enable_wayland = false, check_for_updates = false,\n"
                '  front_end = "Software", enable_tab_bar = false, window_decorations = "NONE",\n'
                "  window_padding = { left = 0, right = 0, top = 0, bottom = 0 },\n"
                "  audible_bell = 'Disabled',\n"
                "}\n"
            )
        cmd = ["wezterm", "--config-file", cfg, "start", "--always-new-process", "--", *py]
        return subprocess.Popen(cmd, env=env, start_new_session=True, stderr=open(os.path.join(work, "stderr.log"), "w"))
    if term == "tmux":
        # A new socket per trace: a server that was just killed can still own the last one.
        global TMUX_N
        TMUX_N += 1
        sock = f"fil-terminals-{os.getpid()}-{TMUX_N}"
        env["FIL_TMUX_SOCKET"] = sock
        conf = os.path.join(work, "tmux.conf")
        with open(conf, "w") as f:
            f.write("set -g history-limit 2000\nset -g default-terminal tmux-256color\nset -s escape-time 0\n"
                    "set -g remain-on-exit off\n" + os.environ.get("TERMINALS_TMUX_CONF", ""))
        base = ["tmux", "-L", sock]
        # A holder session keeps the server alive when the player's session ends.
        subprocess.run(base + ["-f", conf, "new-session", "-d", "-s", "hold", "sleep 3600"], env=env, check=True)
        subprocess.run(base + ["new-session", "-d", "-s", "t", "-x", str(cols), "-y", str(rows),
                               " ".join(shell_quote(a) for a in py)], env=env, check=True)
        return TmuxServer(sock, env)
    raise SystemExit(f"unknown terminal {term}")


def shell_quote(s):
    return "'" + s.replace("'", "'\\''") + "'"


class TmuxServer:
    """A tmux server we started, behind the Popen interface the others give: it is "running"
    while the player's session `t` exists."""

    def __init__(self, sock, env):
        self.sock, self.env = sock, env

    def poll(self):
        r = subprocess.run(["tmux", "-L", self.sock, "has-session", "-t", "t"], env=self.env, capture_output=True)
        return None if r.returncode == 0 else 0

    def kill(self):
        subprocess.run(["tmux", "-L", self.sock, "kill-server"], env=self.env, capture_output=True)


def stop(proc):
    """Ends a terminal we started, by its own PID (its process group for the GUI terminals)."""
    if isinstance(proc, TmuxServer):
        proc.kill()
        return
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except OSError:
        return
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except OSError:
            pass


def diff_hash(diffs):
    """A short hash of a checkpoint's differences. The differences are deterministic text, so a
    listed disagreement is pinned to what it was when it was listed: a different or an added
    difference at the same checkpoint is not the listed one."""
    return hashlib.sha256("\n".join(diffs).encode()).hexdigest()[:12]


INTERMITTENT = set()  # (terminal, trace): answers vary from run to run (declared with `!intermittent`)
SUSPECT = {}  # (trace/check, hash): a signature of a fil bug (declared with `!suspect`)
MAX_RETRIED = 2  # more traces than this needing a retry in one run means the flake rate changed


def load_known(path):
    """{(terminal, trace/check): [(hash or "*", reason)]}. A `trace/*` key with hash `*` covers
    every checkpoint of a trace. Directive lines: `!intermittent TAB terminal TAB trace` (the
    trace may be run again, up to twice, and may report an origin-mode cursor as absolute) and
    `!suspect TAB trace/check TAB hash TAB note` (a difference that is the signature of a
    bug in what fil writes: it fails the run even if a later attempt passes)."""
    known = {}
    INTERMITTENT.clear()
    SUSPECT.clear()
    if os.path.exists(path):
        for line in open(path):
            line = line.rstrip("\n")
            if not line or line.startswith("#"):
                continue
            cols = line.split("\t")
            if cols[0] == "!intermittent":
                INTERMITTENT.add((cols[1], cols[2]))
                continue
            if cols[0] == "!suspect":
                SUSPECT[(cols[1], cols[2])] = cols[3] if len(cols) > 3 else ""
                continue
            term, key, why, h = (cols + ["", "", "", "*"])[:4]
            known.setdefault((term, key), []).append((h, why))
    return known


def lookup(known, term, key, h):
    """(entry, state): the listed entry for this difference, or why there is none."""
    trace = key.partition("/")[0]
    for k in ((term, key), (term, trace + "/*")):
        for eh, why in known.get(k, []):
            if eh in ("*", h):
                return (k, eh), "known"
    if (term, key) in known:
        return None, "changed"
    return None, "new"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("terminal")
    ap.add_argument("traces")
    ap.add_argument("out")
    ap.add_argument("--only", action="append", default=[])
    ap.add_argument("--timeout", type=float, default=90)
    ap.add_argument("--dump", action="store_true", help="keep each check's raw read-back in OUT/dump")
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    result = os.path.join(args.out, f"{args.terminal}.jsonl")
    open(result, "w").close()
    dump = ""
    if args.dump:
        dump = os.path.join(args.out, "dump", args.terminal)
        os.makedirs(dump, exist_ok=True)
    known = {} if os.environ.get("TERMINALS_STRICT") == "1" else load_known(os.environ.get("TERMINALS_KNOWN") or os.path.join(HERE, "known.tsv"))
    global TMP
    TMP = tempfile.mkdtemp(prefix="ft-")

    traces = []
    for fn in sorted(os.listdir(args.traces)):
        if not fn.endswith(".json"):
            continue
        if args.only and not any(o in fn for o in args.only):
            continue
        path = os.path.join(args.traces, fn)
        t = json.load(open(path))
        traces.append({"path": path, "name": t["name"], "cols": t["cols"], "rows": t["rows"], "group": t["group"]})

    started = time.time()
    failed = False
    summary = {"ok": 0, "info": 0, "known": 0, "new": 0, "error": 0, "stale": 0, "retried": 0}
    seen = set()
    ran = {tr["name"] for tr in traces}
    lines = []
    def attempt(tr):
        """Runs one trace in a fresh terminal; returns (records, finished, stderr tail, timed out)."""
        work = os.path.join(args.out, "work", args.terminal, tr["name"])
        shutil.rmtree(work, ignore_errors=True)
        os.makedirs(work)
        before = os.path.getsize(result)
        proc = launch(args.terminal, tr, work, result, dump)
        deadline = time.time() + args.timeout
        while proc.poll() is None and time.time() < deadline:
            time.sleep(0.1)
        timed_out = proc.poll() is None
        stop(proc)
        err_tail = ""
        try:
            with open(os.path.join(work, "stderr.log"), errors="replace") as f:
                err_tail = " | ".join(x.strip()[:160] for x in f.read().splitlines()[-2:] if "PARSE ERROR" not in x)
        except OSError:
            pass
        shutil.rmtree(work, ignore_errors=True)
        with open(result) as f:
            f.seek(before)
            recs = [json.loads(x) for x in f if x.strip()]
        return recs, any(r.get("done") for r in recs), err_tail, timed_out

    def evaluate(tr, recs, done, err_tail, timed_out):
        """Sorts one attempt's records: (lines, counts, matched known entries, failed)."""
        lines, counts, matched, bad = [], dict.fromkeys(summary, 0), set(), False
        unlisted, suspects = {}, []
        for r in recs:
            if "error" in r:
                counts["error"] += 1
                bad = True
                lines.append(f"ERROR   {tr['name']}/{r.get('check', '')}: {r['error']}")
            elif "check" in r and tr["group"] == "canary":
                # Wrong on purpose: it expects another text, another cursor and other
                # attributes, and all three must show.
                joined = " ".join(r["diffs"])
                missing = [k for k, w in (("row", "row "), ("cursor", "cursor ("), ("attributes", "attrs:")) if w not in joined]
                if r["status"] == "ok" or missing:
                    counts["error"] += 1
                    bad = True
                    lines.append(f"ERROR   {tr['name']}/{r['check']}: the deliberately wrong expectation was not fully noticed (missing: {', '.join(missing) or 'all'}); the read-back is blind")
                else:
                    counts["ok"] += 1
            elif "check" in r:
                key = f"{tr['name']}/{r['check']}"
                if r["status"] == "ok":
                    counts["ok"] += 1
                elif r["status"] == "info":
                    counts["info"] += 1
                else:
                    entry, state = lookup(known, args.terminal, key, diff_hash(r["diffs"]))
                    if state == "known":
                        counts["known"] += 1
                        matched.add(entry)
                        lines.append(f"known   {key}: {'; '.join(r['diffs'][:2])}")
                    else:
                        counts["new"] += 1
                        bad = True
                        h = diff_hash(r["diffs"])
                        unlisted[key] = h
                        if (key, h) in SUSPECT:
                            suspects.append(f"SUSPECT {key} [{h}]: {SUSPECT[(key, h)]}; {'; '.join(r['diffs'][:2])}")
                        note = " (a listed checkpoint, but not the listed difference)" if state == "changed" else ""
                        lines.append(f"DIFF    {key}{note} [{h}]: {'; '.join(r['diffs'][:3])}")
        if not done and not any("error" in r for r in recs):
            counts["error"] += 1
            bad = True
            lines.append(f"ERROR   {tr['name']}: the terminal exited without finishing the trace" + (" (timed out)" if timed_out else "") + (f"; its stderr: {err_tail}" if err_tail else ""))
        return lines, counts, matched, bad, unlisted, suspects

    # Some traces are declared intermittent for a terminal (`!intermittent` in known.tsv: WezTerm's
    # scroll regions, origin mode and pending wrap, finding 8). Only those are run again when they
    # show an unlisted difference, up to twice. Every other trace fails on its first one. The
    # evidence is kept: each failed attempt is printed as RETRIED and counted, the same unlisted
    # difference in two attempts fails, a SUSPECT signature fails even if a later attempt passes,
    # and more than MAX_RETRIED retried traces in a run fails.
    retried = 0
    for tr in traces:
        t0 = time.time()
        flaky = (args.terminal, tr["name"]) in INTERMITTENT
        tries = 3 if flaky and os.environ.get("TERMINALS_NO_RETRY") != "1" else 1
        os.environ["FIL_INTERMITTENT"] = "1" if flaky else "0"
        evs = []
        for n in range(tries):
            os.environ["FIL_ATTEMPT"] = str(n + 1)
            evs.append(evaluate(tr, *attempt(tr)))
            if not evs[-1][3] or evs[-1][5]:
                break
        good = next((e for e in evs if not e[3]), None)
        final = good or evs[-1]
        failed_attempts = [e for e in evs if e is not good and e[3]]
        out = list(final[0])
        trace_failed = good is None
        if good is not None and failed_attempts:
            retried += 1
            summary["retried"] += 1
            for e in failed_attempts:
                out += [l.replace("DIFF    ", "RETRIED ", 1) for l in e[0] if l.startswith("DIFF")]
        suspects = [s for e in evs for s in e[5]]
        if suspects:
            trace_failed = True
            out += suspects
        counts_seen = {}
        for e in evs:
            for kv in e[4].items():
                counts_seen[kv] = counts_seen.get(kv, 0) + 1
        for (key, h), c in counts_seen.items():
            if c >= 2:
                trace_failed = True
                out.append(f"REPEATED {key} [{h}]: the same unlisted difference in {c} of {len(evs)} attempts")
        lines.extend(out)
        for k, v in final[1].items():
            summary[k] += v
        seen |= final[2]
        failed = failed or trace_failed
        print(f"{args.terminal} {tr['name']}: {time.time() - t0:.1f}s" + (f" ({len(evs)} attempts)" if len(evs) > 1 else ""), file=sys.stderr, flush=True)
    if retried > MAX_RETRIED:
        failed = True
        lines.append(f"FLAKY   {retried} traces needed a retry in this run (more than {MAX_RETRIED}): the flake rate has changed")

    # A listed difference that no longer occurs: the disagreement was fixed, or the line was wrong.
    for (term, key), entries in sorted(known.items()):
        trace = key.partition("/")[0]
        if term != args.terminal or trace not in ran or key.endswith("/*"):
            continue
        for eh, why in entries:
            if ((term, key), eh) not in seen:
                summary["stale"] += 1
                # A declared-intermittent trace can agree in the accepted run: a warning there.
                failed = failed or (term, trace) not in INTERMITTENT
                lines.append(f"STALE   {key} [{eh}]: listed in known.tsv ({why}) and no longer occurring; remove the line")

    shutil.rmtree(TMP, ignore_errors=True)
    elapsed = time.time() - started
    print("\n".join(lines))
    print(f"{args.terminal}: {len(traces)} traces, {summary['ok']} checks agree, {summary['info']} informational, "
          f"{summary['known']} known disagreements, {summary['new']} NEW disagreements, {summary['stale']} stale entries, {summary['retried']} retried traces, {summary['error']} errors; {elapsed:.0f}s")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
