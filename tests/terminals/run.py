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
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PLAYER = os.path.join(HERE, "player.py")


TMUX_N = 0


def launch(term, trace, work, result, dump):
    """Starts the terminal with the player in it; returns (Popen, cleanup)."""
    py = [sys.executable, "-I", PLAYER, "--term", term, "--trace", trace["path"], "--out", result]
    if dump:
        py += ["--dump", dump]
    cols, rows = trace["cols"], trace["rows"]
    env = dict(os.environ, LANG="C.UTF-8", LC_ALL="C.UTF-8")
    env.pop("TMUX", None)
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
            "-xrm", f"XTerm*printerCommand: cat >> {printed}",
            "-xrm", "XTerm*printAttributes: 2",
            "-xrm", "XTerm*printerFormFeed: true",
            "-xrm", "XTerm*printerExtent: true",
            "-xrm", f"XTerm*printFileImmediate: {immediate}/p",
            "-xrm", "XTerm*printModeImmediate: 2",
            "-xrm", "XTerm*printOptsImmediate: 0",
            "-xrm", "XTerm*VT100.translations: #override <Key>F5: print-immediate()",
            "-e", *py,
        ]
        return subprocess.Popen(cmd, env=env, start_new_session=True)
    if term == "kitty":
        env["LIBGL_ALWAYS_SOFTWARE"] = "1"
        # A unix socket path is limited to about 100 bytes; the work directory can be longer.
        sock = os.path.join(tempfile.gettempdir(), f"fil-kitty-{os.getpid()}.sock")
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
        return subprocess.Popen(cmd, env=env, start_new_session=True)
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
        return subprocess.Popen(cmd, env=env, start_new_session=True)
    if term == "tmux":
        # A new socket per trace: a server that was just killed can still own the last one.
        global TMUX_N
        TMUX_N += 1
        sock = f"fil-terminals-{os.getpid()}-{TMUX_N}"
        env["FIL_TMUX_SOCKET"] = sock
        conf = os.path.join(work, "tmux.conf")
        with open(conf, "w") as f:
            f.write("set -g history-limit 2000\nset -g default-terminal tmux-256color\nset -s escape-time 0\n"
                    "set -g remain-on-exit off\n")
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


def load_known(path):
    known = {}
    if os.path.exists(path):
        for line in open(path):
            line = line.rstrip("\n")
            if not line or line.startswith("#"):
                continue
            term, key, why = (line.split("\t") + ["", "", ""])[:3]
            known[(term, key)] = why
    return known


def is_known(known, term, key):
    """A known entry matches its exact key, or a trace-wide `trace/*` or `*/check` pattern."""
    trace, _, check = key.partition("/")
    for k in ((term, key), (term, trace + "/*"), (term, "*/" + check), (term, "*")):
        if k in known:
            return known[k]
    return None


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
    known = {} if os.environ.get("TERMINALS_STRICT") == "1" else load_known(os.path.join(HERE, "known.tsv"))

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
    summary = {"ok": 0, "info": 0, "known": 0, "new": 0, "error": 0}
    lines = []
    for tr in traces:
        work = os.path.join(args.out, "work", args.terminal, tr["name"])
        shutil.rmtree(work, ignore_errors=True)
        os.makedirs(work)
        t0 = time.time()
        before = os.path.getsize(result)
        proc = launch(args.terminal, tr, work, result, dump)
        deadline = time.time() + args.timeout
        while proc.poll() is None and time.time() < deadline:
            time.sleep(0.1)
        timed_out = proc.poll() is None
        stop(proc)
        shutil.rmtree(work, ignore_errors=True)
        recs = []
        with open(result) as f:
            f.seek(before)
            recs = [json.loads(x) for x in f if x.strip()]
        done = any(r.get("done") for r in recs)
        for r in recs:
            if "error" in r:
                summary["error"] += 1
                failed = True
                lines.append(f"ERROR   {tr['name']}/{r.get('check', '')}: {r['error']}")
            elif "check" in r and tr["group"] == "canary":
                # Wrong on purpose: the read-back must see it.
                if r["status"] == "ok":
                    summary["error"] += 1
                    failed = True
                    lines.append(f"ERROR   {tr['name']}/{r['check']}: the deliberately wrong expectation was not noticed; the read-back is blind")
                else:
                    summary["ok"] += 1
            elif "check" in r:
                key = f"{tr['name']}/{r['check']}"
                if r["status"] == "ok":
                    summary["ok"] += 1
                elif r["status"] == "info":
                    summary["info"] += 1
                else:
                    why = is_known(known, args.terminal, key)
                    if why is not None:
                        summary["known"] += 1
                        lines.append(f"known   {key}: {'; '.join(r['diffs'][:2])}")
                    else:
                        summary["new"] += 1
                        failed = True
                        lines.append(f"DIFF    {key}: {'; '.join(r['diffs'][:3])}")
        if not done and not any("error" in r for r in recs):
            summary["error"] += 1
            failed = True
            lines.append(f"ERROR   {tr['name']}: the terminal exited without finishing the trace" + (" (timed out)" if timed_out else ""))
        print(f"{args.terminal} {tr['name']}: {time.time() - t0:.1f}s", file=sys.stderr, flush=True)

    elapsed = time.time() - started
    print("\n".join(lines))
    print(f"{args.terminal}: {len(traces)} traces, {summary['ok']} checks agree, {summary['info']} informational, "
          f"{summary['known']} known disagreements, {summary['new']} NEW disagreements, {summary['error']} errors; {elapsed:.0f}s")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
