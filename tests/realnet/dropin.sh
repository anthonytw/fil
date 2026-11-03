#!/usr/bin/env bash
# The drop-in suite's pieces for the real-network harness (docs/m1/dropin.md). Sourced by udp.sh
# after lib.sh and fil.sh, never run.
#
# 1. dp_battery: one list of everyday ssh work, run through ANY client on a PTY (the real `fil`
#    or `ssh -tt`), recording what the screen showed, so the two can be compared like for like.
# 2. dp_vim_*: typing into vim while the network misbehaves.
#
# Typed text goes through ptydrive, where a backslash starts an escape: a literal backslash is
# written `\\`. Most commands here avoid backslashes altogether.

# The editor on this machine ("vim" or "vi"); empty if there is none.
dp_editor() {
  local e
  for e in vim vi; do
    if command -v "$e" > /dev/null 2>&1; then
      printf '%s' "$e"
      return 0
    fi
  done
  return 0
}

# dp_snap NAME FILE [GREP_ARGS...]: the client's screen (filtered) saved in FILE.
dp_snap() {
  local n=$1 f=$2
  shift 2
  if [[ $# -gt 0 ]]; then
    fc_screen "$n" | grep "$@" > "$f" || true
  else
    fc_screen "$n" > "$f"
  fi
}

# dp_battery NAME TAG: the everyday work, through the client NAME (already started, its shell
# ready). It leaves one file per step in $DP_OUT/TAG.STEP and notes the timings. The shell is
# the one of the `rnfil` account on the server; the steps use /tmp/dropin-TAG.
dp_battery() {
  local n=$1 tag=$2 ed t0 t1 d=/tmp/dropin-$2
  mkdir -p "$DP_OUT"
  # An ordinary window: 30 rows (ptydrive's 200 would show a whole page of everything).
  fc "$n" resize 30 100 > /dev/null
  fc_line "$n" "PS1='fil\$ '"
  fc_line "$n" "rm -rf $d; mkdir $d && cd $d"
  fc_ready "$n"

  # vim: open, edit, :wq; the file on disk has the edit.
  ed=$(dp_editor)
  if [[ -n $ed ]]; then
    fc_line "$n" 'echo alpha > e.txt'
    fc_line "$n" "$ed e.txt"
    fc_wait "$n" '1L, 6B' 20
    fc_type "$n" 'Ofrom the editor\e'
    fc_wait "$n" 'from the editor' 10
    fc_type "$n" ':wq\r'
    sleep 1 # vim discards what is typed ahead while it leaves
    fc_ready "$n"
    # The whole file, not a piece of it: exactly what was typed above alpha.
    fc_line "$n" '{ echo from the editor; echo alpha; } | cmp -s - e.txt && echo VIMEXACT-$((6*7))'
    fc_wait "$n" 'VIMEXACT-42' 10
    note "$tag.vim=ok"
  else
    note "$tag.vim=SKIP(no vim or vi)"
  fi

  # less: page forward, search, quit.
  if command -v less > /dev/null 2>&1; then
    # Lines N1 .. N500: the typed commands never contain a line of the file, so a match on the
    # screen is less's output.
    fc_line "$n" "seq -f 'N%g' 1 500 > nums.txt"
    fc_line "$n" 'less nums.txt'
    fc_wait "$n" 'N29' 10
    fc_type "$n" ' '
    fc_wait "$n" 'N58' 10
    fc_type "$n" '/^N333$\r'
    fc_wait "$n" 'N361' 10
    dp_snap "$n" "$DP_OUT/$tag.less"
    fc_type "$n" 'q'
    sleep 1
    fc_ready "$n"
    note "$tag.less=ok"
  fi

  # A long, fast flood: complete and in order.
  t0=$(now_ms)
  fc_line "$n" 'seq 1 200000; echo FLOOD-$((6*7))'
  fc_wait "$n" 'FLOOD-42' 180
  t1=$(now_ms)
  note "$tag.flood_ms=$((t1 - t0))"
  dp_snap "$n" "$DP_OUT/$tag.flood" -E '^[0-9]+$'
  [[ $(tail -n 1 "$DP_OUT/$tag.flood") == 200000 ]] || fail "$tag: the flood does not end with 200000: $(tail -n 3 "$DP_OUT/$tag.flood")"

  # `cat` of a 4 MB file: ssh delivers every line to the scrollback. fil may skip some with a gap
  # marker (DESIGN.md catch-up); count what reached the terminal.
  fc_line "$n" "seq -f 'LINE-%06g 0123456789abcdefghij0123456789abcdefghij0123456789abcdefghij' 1 50000 > big.txt"
  fc_line "$n" 'cat big.txt; echo CATEND-$((6*7))'
  fc_wait "$n" 'CATEND-42' 120
  fc_wait "$n" 'LINE-050000' 20
  note "$tag.cat_lines_received=$(fc "$n" raw | grep -a -o 'LINE-[0-9][0-9][0-9][0-9][0-9][0-9]' | wc -l | tr -d ' ') $tag.cat_gap_markers=$(fc "$n" raw | grep -a -c 'fil: skipped' || true)"

  # Ctrl-C during a flood: the next command's output comes back.
  fc_line "$n" 'yes'
  sleep 3
  t0=$(now_ms)
  fc_type "$n" '\x03'
  fc_line "$n" 'echo ALIVE-$((6*7))'
  fc_wait "$n" 'ALIVE-42' 90
  t1=$(now_ms)
  note "$tag.ctrl_c_flood_ms=$((t1 - t0))"

  # A resize reaches the remote. (A command typed at the same instant can see the old size: that
  # race is the Pi suite's expected failure `resize_order`, docs/m1/dropin.md finding 1; over
  # ptydrive the keystrokes cannot be that close, so this step looks at the settled size.)
  fc "$n" resize 40 120 > /dev/null
  sleep 1
  fc_line "$n" 'echo SZ-$(stty size | tr " " x)'
  fc_wait "$n" 'SZ-40x120' 15
  note "$tag.resize=ok"

  # Unicode and wide characters (typed as UTF-8). The result carries a computed 42, so the typed
  # line (which has the arithmetic) is never mistaken for the output; the last UNI- row is it.
  fc_line "$n" 'echo UNI-é-日本語-🎉-$((6*7))'
  fc_wait "$n" 'UNI-é-日本語-🎉-42' 10
  fc_screen "$n" | grep -F 'UNI-' | tail -n 1 > "$DP_OUT/$tag.unicode.out"

  # Colors: the escape reaches the terminal.
  fc_line "$n" 'mkdir cd1; ls --color=always -d cd1'
  fc_wait_raw "$n" '\e[01;34mcd1' 10

  # The terminal title.
  fc_line "$n" 'printf "\\033]0;TTL-%s\\007\\n" 42'
  fc_wait_raw "$n" ']0;TTL-42' 10

  # Ctrl-Z, fg, Ctrl-C. The job prints RESUMED-42 when it gets SIGCONT (the typed text holds
  # `RESUMED-%s 42`, never the result), so the output proves that `fg` resumed it.
  local job="bash -c \"trap 'printf RESUMED-%s 42; echo' CONT; while :; do sleep 0.2; done\""
  fc_line "$n" "$job"
  sleep 1
  fc_type "$n" '\x1a'
  fc_wait "$n" 'Stopped' 10
  fc_line "$n" 'fg'
  fc_wait "$n" 'RESUMED-42' 10
  fc_type "$n" '\x03'
  sleep 0.3 # as a person: readline can drop a character typed at the instant of a Ctrl-C
  fc_line "$n" 'jobs > j.txt; echo JOBS-$(wc -l < j.txt | tr -d " ")'
  fc_wait "$n" 'JOBS-0' 10
  note "$tag.job_control=ok"

  # The shell's exit status reaches the client.
  fc_line "$n" 'cd; rm -rf '"$d"
  fc_line "$n" 'exit 7'
  fc_expect_exit "$n" 7 20
  note "$tag.exit_status=7"
}

# dp_start_vim NAME: in the client's shell: a scratch directory, a file, the editor
# open on it, in insert mode on a new last line. Sets DP_ED. Returns 1 if there is no editor.
dp_start_vim() {
  local n=$1
  DP_ED=$(dp_editor)
  [[ -n $DP_ED ]] || return 1
  fc_line "$n" "PS1='fil\$ '"
  fc_line "$n" 'cd $(mktemp -d) && echo start > e.txt'
  fc_ready "$n"
  fc_line "$n" "$DP_ED e.txt"
  fc_wait "$n" '1L, 6B' 20
  fc_type "$n" 'o'
}

# dp_finish_vim NAME FIRST LAST: leave insert mode, :wq, and the whole file must be exactly
# `start`, ROW-FIRST .. ROW-LAST once each in order, and the empty line the last Enter made. (A
# doubled keystroke or a replayed tail makes `cmp` fail; a substring match would not.)
dp_finish_vim() {
  local n=$1
  fc_type "$n" '\e:wq\r'
  sleep 1 # vim discards what is typed ahead while it leaves
  fc_ready "$n" 60
  fc_line "$n" "{ echo start; seq -f 'ROW-%g' $2 $3; echo; } | cmp -s - e.txt && echo EXACT-\$((6*7))"
  fc_wait "$n" 'EXACT-42' 20
}
