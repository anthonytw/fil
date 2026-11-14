#!/usr/bin/env bash
# The prediction cells of the latency lab (docs/m3/plan.md §5.5; built by M3's T3, run for the
# record by V1): key-to-glyph latency through the real `fil` client, with its prediction off,
# adaptive and always, against `ssh -tt` as the control. `keyprobe keys --screen` types into a
# raw `cat` (or at a shell's prompt) and times each key by when its glyph first shows in its cell
# on an emulated screen, so a written guess counts; the final screen must hold exactly the keys.
# L1's topology (latency.sh, udp.sh): network namespaces, NAT, tc netem, the runner's own sshd in
# the server namespace; root and the same tools. NOT part of `run.sh all`: `just realnet predict`,
# or the linux-check workflow with `-f suite=latency`. Run: predict.sh [SCENARIO...]
#
#   p_keys    fil on the pipe and on QUIC, each prediction mode, and `ssh -tt -o
#             ObscureKeystrokeTiming=no`; 25, 75 and 200 ms each way, +-5 ms
#   p_fast    fil on the pipe and on QUIC (a raw `cat`) and at a bash prompt, adaptive, 2.5 ms
#             each way: nothing written (done-when 4)
#   p_loss    fil on QUIC, adaptive, 75 ms each way, 2 % and 5 % loss; keys later than one round
#             trip plus 250 ms (the glitch bound, PC-1) are noted
#   p_roam    fil on QUIC, adaptive, 75 ms each way, three NAT moves while the keys are typed
#   p_shells  fil, adaptive, 75 ms each way: typing at the prompt of bash, zsh and fish (those
#             installed; bash is required)
#   p_ahead   fil on the pipe and on QUIC, adaptive, 75 and 200 ms each way: the lines typed
#             straight through (`keyprobe --ahead`), at 20 keys/s and at 5: the hard case for
#             prediction (docs/m3/latency.md "Lines"). Judged as p_keys but for done-when 3; how
#             many keys a guess showed is recorded, not a bar
#
# **The prediction mode.** Every fil cell reads a config file of its own: with `[defaults] predict =
# "off"` when the client has the flag (the built-in default is adaptive), so an off cell is off
# whatever else happens; empty when it has none (0.1.0 never predicts and refuses a `predict` key,
# N9). The client
# also gets `--predict MODE` when `fil --help` lists that flag (M3's E1b). Until then fil never
# predicts, so the `off` cells run and the others are noted as not run; when the flag arrives they
# run with nothing changed here. A cell named adaptive in the list above runs as `off` until then
# (its tag says which ran). A help text that does not look like fil's fails the setup, and a client
# that logs prediction counts while no flag was found fails its cell: the detection is wrong.
#
# **What a cell checks.** Every key's glyph appeared (`lost=0`; a KNOWN-BUG otherwise, as in
# latency.sh); the final screen holds the keys and nothing after them, with the cursor right after
# the last (fails otherwise); the path (a QUIC cell on QUIC before the first measured key, a pipe
# cell never on QUIC). The fil client then detaches (`~.`), so it logs its prediction counts at
# exit (debug); with prediction on they must be there, with no visible misprediction, no more
# `fast` keys than guesses written, and in p_fast nothing written; with it off (and in the ssh
# cells) nothing written and no `fast` key, which also checks that half the round trip still
# separates a guess from an echo (raise PR_JITTER_MS to half the delay and it would not).
#
# **Prediction helps** (done-when 3): in p_keys, every adaptive cell at 75 ms each way and more must
# have written guesses, with keys shown by one (`fast`), and their p50 (`fast_p50_ms`) under 5 ms.
#
# **Measured**, per cell, in $OUT/results-predict-cells.tsv (and the job summary): key-to-glyph p50
# and p99 over all measured keys, and over the `fast` ones (shown within half the round trip: the
# keys a guess showed, as no echo comes back that soon); `vanished`, the keys whose glyph went away
# after it first showed (a guess taken back and redrawn by the echo: their time is still the
# guess's); the byte-echo p50 of the same run, with prediction off only (`-` otherwise: the client
# writes a guess's glyph as a byte, so the bytes line then times the guess, not the echo); the
# client's counts (guesses written, visible mispredictions). Knobs (environment):
#   PR_RATE=20 PR_COUNT=200   keys per second and measured keys per cell (10 more are a warm-up)
#   PR_LINE=40                keys per line into a raw `cat` (`keyprobe --line`): Enter after each
#                             line, a wait for its echo and 200 ms more, as a person types commands
#                             (0: one line through the margin, which no one types: see
#                             docs/m3/latency.md "Lines"; p_ahead does not wait)
#   PR_JITTER_MS=5            netem jitter each way (not in p_fast)
#
# fish runs as `fish --no-config --private`: the `\r~.` that ends a cell runs the typed line at the
# prompt, and a fish that keeps history would offer it back as an autosuggestion on the next run
# (every key lost). docs/m3/latency.md is where the numbers are recorded (V1). The client
# logs here are debug logs, kept in the run's artifact: fine for the throwaway runner, but debug
# logs from the Pis must never be uploaded anywhere public.
set -uo pipefail
# shellcheck source=latency.sh
source "$(dirname "${BASH_SOURCE[0]}")/latency.sh"

PR_RATE=${PR_RATE:-20}
PR_COUNT=${PR_COUNT:-200}
PR_LINE=${PR_LINE:-40}
PR_JITTER_MS=${PR_JITTER_MS:-5}
PR_CELLS=$OUT/results-predict-cells.tsv
PR_DELAYS=(25 75 200)
# The client config of every fil cell: prediction off unless `--predict` says otherwise.
PR_CONFIG=$PD/predict-off.toml

# ---------------------------------------------------------------------------------------------
# Setup, teardown, the network

pr_setup() {
  udp_setup
  client_exec_on x mkdir -p "$PD"
  # Whether the client under test has `--predict` (written down for the scenario processes, which
  # read the part's env file).
  local help has=0
  if ! help=$(client_exec_on x fil --help 2>&1) || [[ $help != *--transport* ]]; then
    echo "realnet: \`fil --help\` failed or is not fil's (no --transport), so --predict cannot be detected: $help" >&2
    exit 1
  fi
  if grep -qE -- '(^|[[:space:],])--predict([[:space:]=<,]|$)' <<< "$help"; then has=1; fi
  # The off cells' prediction setting, whatever the flag detection says. A build without the flag
  # (0.1.0) has no `predict` key either (N9: the parser refuses it), and never predicts: an empty
  # config. The key is written only for a build that has the flag.
  if [[ $has == 1 ]]; then
    printf '[defaults]\npredict = "off"\n' > "$OUT/run/predict-off.toml"
  else
    : > "$OUT/run/predict-off.toml"
  fi
  as_root install -o rnclient -g rnclient -m 600 "$OUT/run/predict-off.toml" "$PR_CONFIG"
  { cat "$OUT/udp.env"; printf 'PR_HAS_FLAG=%s\n' "$has"; } > "$OUT/predict.env"
  if [[ $has == 1 ]]; then
    log "fil has --predict: every prediction mode runs"
  else
    log "fil has no --predict yet (M3's E1b): only the cells with prediction off run"
  fi
  printf 'cell\ttransport\tpredict\tdelay_ms\tloss\tsent\tshown\tlost\tp50_ms\tp99_ms\tmax_ms\tfast\tfast_p50_ms\tvanished\tbytes_p50_ms\twritten\tmispredicted\n' > "$PR_CELLS"
}

pr_teardown() {
  pr_cells_summary
  udp_teardown "$@"
}

pr_cells_summary() {
  [[ -n ${GITHUB_STEP_SUMMARY:-} && -s $PR_CELLS ]] || return 0
  {
    echo '### prediction cells: key-to-glyph (ms)'
    echo
    echo 'bytes p50 is the byte-echo time, with prediction off only (with it on, the bytes line times the guess).'
    echo
    echo '| cell | transport | predict | delay | loss | sent | shown | lost | p50 | p99 | max | fast | fast p50 | vanished | bytes p50 | written | mispredicted |'
    echo '|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|'
    tail -n +2 "$PR_CELLS" | awk -F'\t' '{ printf "|"; for (i = 1; i <= NF; i++) printf " %s |", $i; printf "\n" }'
  } >> "$GITHUB_STEP_SUMMARY"
}

# pr_net DELAY_MS JITTER_MS [LOSS_PERCENT]: netem on every router interface, so each direction
# gets DELAY_MS. `rate` keeps netem's jitter in order (s_netem_reorder_probe).
pr_net() {
  reset_net
  local d args=(delay "${1}ms")
  [[ $2 == 0 ]] || args+=("${2}ms")
  args+=(rate 1gbit limit 100000)
  [[ -z ${3:-} ]] || args+=(loss "${3}%")
  for d in "${NETEM_DEVS[@]}"; do
    nsx "$NS_R" tc qdisc replace dev "$d" root netem "${args[@]}"
  done
  log "network: ${args[*]} on each direction"
}

# pr_mode MODE: MODE if the client can be told it, else off (the prediction fil has without the flag).
pr_mode() { if [[ ${PR_HAS_FLAG:-0} == 1 ]]; then printf '%s' "$1"; else printf off; fi; }

# The modes p_keys runs.
pr_modes() { if [[ ${PR_HAS_FLAG:-0} == 1 ]]; then echo off adaptive always; else echo off; fi; }

# ---------------------------------------------------------------------------------------------
# A cell

# pr_kp TAG KIND MODE DELAY_MS KEYPROBE_OPTIONS...: keyprobe --screen in the client namespace,
# typing through KIND: ssh (`ssh -tt -o ObscureKeystrokeTiming=no`), pipe, quic or auto (fil, with
# `--config` with prediction off, and `--predict MODE` when it has the flag; the session is named
# TAG). The options pick the program
# (`--shell` for a raw `cat`) and may override the rate and the counts. Output in
# $OUT/run/SCENARIO-TAG.keyprobe (and .err); the fil client logs at debug into a state directory of
# its own. Returns keyprobe's status.
pr_kp() {
  local tag=$1 kind=$2 mode=$3 delay=$4 settle=500 flag=(--config "$PR_CONFIG") cmd lines=()
  shift 4
  # `--shell` (a raw `cat`) is typed into in lines; a shell's prompt (`--setup`) is not: the
  # Enter is `\r\n`, which would run the line there.
  if [[ " $* " == *" --shell "* && $PR_LINE -gt 0 ]]; then lines=(--line "$PR_LINE"); fi
  if [[ ${PR_HAS_FLAG:-0} == 1 ]]; then flag+=(--predict "$mode"); fi
  case $kind in
    ssh) cmd=(ssh -tt -o ObscureKeystrokeTiming=no srv) ;;
    pipe) cmd=(fil "${flag[@]}" --transport pipe srv "$tag") ;;
    quic) cmd=(fil "${flag[@]}" --transport direct srv "$tag") settle=3000 ;;
    auto) cmd=(fil "${flag[@]}" srv "$tag") settle=3000 ;;
    *) fail "pr_kp: unknown kind $kind" ;;
  esac
  log "=== cell $tag: $kind, prediction $mode, ${delay} ms each way"
  # `~.` after a carriage return: fil detaches (and ssh disconnects) by itself.
  client_exec_on x timeout 600 env FIL_LOG=debug "XDG_STATE_HOME=$PD/state-$SCENARIO-$tag" \
    keyprobe keys --screen --rate "$PR_RATE" --count "$PR_COUNT" --warmup 10 --settle-ms "$settle" \
    --fast-ms "$delay" "${lines[@]}" --quit $'\r~.' "$@" -- "${cmd[@]}" \
    > "$OUT/run/$SCENARIO-$tag.keyprobe" 2> "$OUT/run/$SCENARIO-$tag.err"
}

# pr_stats TAG: the fil client's prediction counts from its log (the `prediction` line it writes
# at debug when it ends). Sets PR_WRITTEN and PR_MISS (visible mispredictions: every reason
# counted), both `-` when the log has no such line.
pr_stats() {
  local line
  PR_WRITTEN=- PR_MISS=-
  line=$(fc_log "$1" | grep -F 'PredictStats' | tail -n 1 || true)
  [[ -n $line ]] || return 0
  PR_WRITTEN=$(grep -oE '[{ ]written: [0-9]+' <<< "$line" | head -n 1 | grep -oE '[0-9]+$' || true)
  PR_MISS=$(grep -oE 'Mispredicted \{[^}]*\}' <<< "$line" | grep -oE '[0-9]+' | awk '{ s += $1 } END { print s + 0 }')
  [[ -n $PR_WRITTEN ]] || fail "$1: cannot read the counts from the client's prediction line: $line"
}

# pr_record TAG KIND MODE DELAY_MS LOSS [quiet]: reads the cell's keyprobe lines and the client's
# counts, records the cell, and checks it (see the top of the file). `quiet`: nothing may be
# written even with prediction on (p_fast).
pr_record() {
  local tag=$1 kind=$2 mode=$3 delay=$4 loss=$5 expect=${6:-} line bytes
  line=$(last_line "$OUT/run/$SCENARIO-$tag.keyprobe" 'KEYPROBE mode=screen ')
  bytes=$(last_line "$OUT/run/$SCENARIO-$tag.keyprobe" 'KEYPROBE-BYTES ')
  [[ -n $line ]] || fail "$tag: keyprobe printed no result: $(tail -n 5 "$OUT/run/$SCENARIO-$tag.err" "$OUT/run/$SCENARIO-$tag.keyprobe" 2> /dev/null)"
  PR_WRITTEN=- PR_MISS=-
  if [[ $kind != ssh ]]; then
    lat_save_log "$tag"
    pr_stats "$tag"
    cx_fk "$tag"
  fi
  local sent shown lost p50 p99 max fast vanished bytes_p50=-
  fast=$(kv "$line" fast)
  vanished=$(kv "$line" vanished)
  # With prediction on, the bytes line times the guess's byte, not the echo.
  if [[ $mode == off || $kind == ssh ]]; then bytes_p50=$(kv "$bytes" p50_ms || echo -); fi
  sent=$(kv "$line" sent)
  shown=$(kv "$line" echoed)
  lost=$(kv "$line" lost)
  p50=$(kv "$line" p50_ms)
  p99=$(kv "$line" p99_ms)
  max=$(kv "$line" max_ms)
  PR_FAST=$fast PR_FAST_P50=$(kv "$line" fast_p50_ms) PR_MAX=$max
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$tag" "$kind" "$mode" "$delay" "$loss" \
    "$sent" "$shown" "$lost" "$p50" "$p99" "$max" "$fast" "$PR_FAST_P50" "$vanished" \
    "$bytes_p50" "$PR_WRITTEN" "$PR_MISS" >> "$PR_CELLS"
  note "$tag.p50_ms=$p50 $tag.p99_ms=$p99 $tag.max_ms=$max $tag.fast=$fast $tag.vanished=$vanished $tag.lost=$lost $tag.bytes_p50_ms=$bytes_p50 $tag.written=$PR_WRITTEN $tag.mispredicted=$PR_MISS"

  case $kind in
    pipe | quic) lat_check_path "$tag" "$kind" "$(kv "$line" first_key_ms)" ;;
  esac
  [[ $(kv "$line" final_wrong) == 0 && $(kv "$line" final_extra) == 0 && $(kv "$line" final_cursor) == 1 ]] \
    || fail "$tag: the final screen is not the keys typed: $line"
  ok "$tag: the final screen holds the keys typed and nothing after them"
  if [[ $kind != ssh && ${PR_HAS_FLAG:-0} != 1 && $PR_WRITTEN != - ]]; then
    fail "$tag: the client logged prediction counts, but no --predict was found in \`fil --help\`: fix the detection"
  fi
  if [[ $kind == ssh || $mode == off ]]; then
    [[ $PR_WRITTEN == - || $PR_WRITTEN == 0 ]] || fail "$tag: $PR_WRITTEN guesses written with prediction off"
    # No guess, so a key shown within half the round trip was an echo: the split does not hold.
    [[ $fast == 0 ]] || fail "$tag: $fast keys shown within $delay ms with no prediction: half the round trip no longer separates guesses from echoes"
  else
    [[ $PR_WRITTEN != - ]] || fail "$tag: no prediction counts in the client log (quit=$(kv "$line" quit)): $(fc_log "$tag" | tail -n 3)"
    [[ $PR_MISS == 0 ]] || fail "$tag: $PR_MISS visible mispredictions ($PR_WRITTEN written)"
    [[ $expect != quiet || $PR_WRITTEN == 0 ]] || fail "$tag: $PR_WRITTEN guesses written on a fast link"
    [[ $fast -le $PR_WRITTEN ]] || fail "$tag: $fast keys shown within $delay ms but only $PR_WRITTEN guesses written: some were echoes"
    ok "$tag: $PR_WRITTEN guesses written ($fast keys shown by one, $vanished vanished), no visible misprediction"
  fi
  if [[ $lost != 0 ]]; then known_bug "$tag: $lost of $sent keys never showed on the screen"; fi
}

# pr_cell TAG KIND MODE DELAY_MS LOSS [quiet] -- KEYPROBE_OPTIONS...: pr_kp, then pr_record.
pr_cell() {
  local tag=$1 kind=$2 mode=$3 delay=$4 loss=$5 expect=
  shift 5
  if [[ ${1:-} == quiet ]]; then
    expect=quiet
    shift
  fi
  [[ ${1:-} == -- ]] && shift
  pr_kp "$tag" "$kind" "$mode" "$delay" "$@" \
    || fail "$tag: keyprobe failed: $(tail -n 5 "$OUT/run/$SCENARIO-$tag.err" "$OUT/run/$SCENARIO-$tag.keyprobe" 2> /dev/null)"
  pr_record "$tag" "$kind" "$mode" "$delay" "$loss" ${expect:+"$expect"}
}

# pr_helps TAG DELAY_MS: done-when 3, on the cell pr_record just recorded (an adaptive cell at 75 ms
# each way or more): guesses written, keys shown by one, and those shown under 5 ms at the p50.
pr_helps() {
  local tag=$1 delay=$2
  [[ $delay -ge 75 ]] || return 0
  [[ $PR_WRITTEN =~ ^[0-9]+$ && $PR_WRITTEN -gt 0 && $PR_FAST =~ ^[0-9]+$ && $PR_FAST -gt 0 ]] \
    || fail "$tag: prediction did not help at ${delay} ms each way: written=$PR_WRITTEN fast=$PR_FAST"
  awk -v p="$PR_FAST_P50" 'BEGIN { exit !(p + 0 < 5) }' \
    || fail "$tag: the keys a guess showed took $PR_FAST_P50 ms at the p50 (done-when 3: under 5 ms)"
  ok "$tag: $PR_FAST keys shown by a guess, p50 $PR_FAST_P50 ms (done-when 3)"
}

# ---------------------------------------------------------------------------------------------
# The cells

p_keys() {
  local d kind mode
  if [[ ${PR_HAS_FLAG:-0} != 1 ]]; then note "not_run=adaptive,always (fil has no --predict until M3's E1b)"; fi
  for d in "${PR_DELAYS[@]}"; do
    pr_net "$d" "$PR_JITTER_MS"
    pr_cell "k_ssh_$d" ssh - "$d" 0 -- --shell
    for kind in pipe quic; do
      for mode in $(pr_modes); do
        pr_cell "k_${kind}_${mode}_$d" "$kind" "$mode" "$d" 0 -- --shell
        if [[ $mode == adaptive ]]; then pr_helps "k_${kind}_${mode}_$d" "$d"; fi
      done
    done
  done
}

p_fast() {
  local kind mode
  mode=$(pr_mode adaptive)
  pr_net 2.5 0
  for kind in pipe quic; do
    pr_cell "f_${kind}_$mode" "$kind" "$mode" 2.5 0 quiet -- --shell
  done
  # Done-when 4 is about shells: typing at a bash prompt, too.
  pr_cell "f_bash_$mode" auto "$mode" 2.5 0 quiet -- \
    --setup "PS1='\$ '; printf 'KP-READY-%s\\n' \$((6*7))" --ready KP-READY-42 \
    --rate 10 --count 55 --warmup 5
}

p_loss() {
  local loss mode
  mode=$(pr_mode adaptive)
  for loss in 2 5; do
    pr_net 75 "$PR_JITTER_MS" "$loss"
    pr_cell "l_quic_${mode}_$loss" quic "$mode" 75 "$loss" -- --shell
    # Glitches (PC-1: an echo more than 250 ms late): the slowest key against one round trip plus
    # 250 ms. Recorded, not a bar.
    note "l_quic_${mode}_$loss.glitch=$(awk -v m="$PR_MAX" 'BEGIN { if (m !~ /^[0-9]+(\.[0-9]+)?$/) print "unknown"; else print (m + 0 > 150 + 250) ? "yes" : "no" }') (max $PR_MAX ms against 400 ms)"
  done
}

# p_roam: three NAT moves (a new port, another, a new address) 5 s apart while 400 keys (20 s)
# are typed over QUIC; each move is checked in conntrack, and all three must fall inside the
# typing.
p_roam() {
  local mode tag spec kp rc=0 first moves=() m
  mode=$(pr_mode adaptive)
  tag=r_quic_$mode
  pr_net 75 "$PR_JITTER_MS"
  # shellcheck disable=SC2034 # nat_move (udp.sh) reads it: no UDP rule of its own yet
  NAT_UDP=
  pr_kp "$tag" quic "$mode" 75 --shell --count 390 &
  kp=$!
  # The typing starts about the settle (3 s) after the client is on QUIC and the shell is ready.
  printf -v "FC_WHERE_$tag" x
  wait_log "$tag" 'Switched(Direct)' 60
  sleep 5
  for spec in "$PUB1:31000-31999" "$PUB1:32000-32999" "$PUB3:33000-33999"; do
    nat_move "$spec"
    moves+=("$(now_ms)")
    nat_expect "$spec"
    sleep 5
  done
  wait "$kp" || rc=$?
  [[ $rc -eq 0 ]] || fail "$tag: keyprobe failed: $(tail -n 5 "$OUT/run/$SCENARIO-$tag.err" 2> /dev/null)"
  first=$(kv "$(last_line "$OUT/run/$SCENARIO-$tag.keyprobe" 'KEYPROBE mode=screen ')" first_key_ms)
  [[ $first =~ ^[0-9]+$ ]] || fail "$tag: no first key time: $(cat "$OUT/run/$SCENARIO-$tag.keyprobe")"
  for m in "${moves[@]}"; do
    [[ $m -gt $first && $m -lt $((first + 400 * 1000 / PR_RATE)) ]] \
      || fail "$tag: a NAT move at $m ms is outside the typing ($first ms, for $((400 / PR_RATE)) s)"
  done
  note "moves_after_first_key_ms=$(for m in "${moves[@]}"; do printf '%s,' $((m - first)); done)"
  pr_record "$tag" quic "$mode" 75 0
}

# p_shells: typing at the prompt of each shell that is installed (60 keys at 10 keys/s, short
# enough for one line after any prompt), the final screen exact.
p_shells() {
  local mode sh setup ran=() missing=()
  mode=$(pr_mode adaptive)
  pr_net 75 "$PR_JITTER_MS"
  for sh in bash zsh fish; do
    if ! command -v "$sh" > /dev/null; then
      missing+=("$sh")
      continue
    fi
    # The typed line holds the arithmetic, never the result (keyprobe's `--shell` rule); the
    # prompt comes after the ready text.
    case $sh in
      bash) setup="PS1='\$ '; printf 'KP-READY-%s\\n' \$((6*7))" ;;
      zsh) setup="printf 'KP-READY-%s\\n' \$((6*7)); exec zsh -f" ;;
      fish) setup="printf 'KP-READY-%s\\n' \$((6*7)); exec fish --no-config --private" ;;
    esac
    pr_cell "s_${sh}_$mode" auto "$mode" 75 0 -- --setup "$setup" --ready KP-READY-42 \
      --rate 10 --count 55 --warmup 5
    ran+=("$sh")
  done
  [[ ${#missing[@]} -eq 0 ]] || note "shells_missing=$(IFS=,; echo "${missing[*]}")"
  [[ " ${ran[*]} " == *" bash "* ]] || fail "bash is required for p_shells"
  ok "typed at the prompt of: ${ran[*]}"
}

# p_ahead: the lines typed straight through, the next command typed before the last one's output
# is back. Every key must still show and nothing be shown wrong; the keys a guess showed (`fast`)
# are recorded as a share of the keys, the number prediction loses when the typing outruns the
# echoes (docs/m3/latency.md "Lines").
p_ahead() {
  local d kind mode rate tag
  mode=$(pr_mode adaptive)
  for d in 75 200; do
    pr_net "$d" "$PR_JITTER_MS"
    for kind in pipe quic; do
      for rate in "$PR_RATE" 5; do
        tag="t_${kind}_${mode}_${d}_r$rate"
        pr_cell "$tag" "$kind" "$mode" "$d" 0 -- --shell --ahead --rate "$rate"
        note "$tag.shown_by_a_guess=$PR_FAST/$PR_COUNT"
      done
    done
  done
}

PR_SCENARIOS=(p_keys p_fast p_loss p_roam p_shells p_ahead)

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  part_main predict pr_setup pr_teardown "${PR_SCENARIOS[@]}" -- "$@"
fi
