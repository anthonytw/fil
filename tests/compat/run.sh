#!/usr/bin/env bash
# Old against new within 1.x (docs/v1/wire.md, DESIGN.md section 8.5 "Compatibility").
#
# 1. Once v1.0.0 is tagged, the 1.0 wire baseline (crates/fil-proto/tests/golden/wire-1.0.txt)
#    must be the tagged one, byte for byte: nobody may re-take it after the freeze.
# 2. A bounded set of the tags in tests/compat/tags.txt (v1.0.0, and the newest patch of each of
#    the two newest minors after it: at most three, so the job's time stays bounded however many
#    releases there are) is built (`--locked`) and run against HEAD with the drop-in quick set,
#    against a private daemon, twice: the old `fil` with HEAD's daemon, then HEAD's `fil` with
#    the old daemon.
#
# linux-check's `compat` job runs it (suites extended and all); it needs the tags (a full clone).
# With no tag listed it says so and passes.
#
#   tests/compat/run.sh [TAGS_FILE]
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
list=${1:-$root/tests/compat/tags.txt}
target=${CARGO_TARGET_DIR:-$root/target}
work=$target/compat

cd "$root"
tests/compat/anchor.sh

tags=()
lines=()
while IFS= read -r line || [ -n "$line" ]; do
  line=${line%%#*}
  line=$(printf '%s' "$line" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')
  [ -n "$line" ] || continue
  tag=${line%% *}
  extra=${line#"$tag"}
  if ! [[ $tag =~ ^v1\.[0-9]+\.[0-9]+$ ]]; then
    echo "compat: '$tag' in $list is not a 1.x tag (vMAJOR.MINOR.PATCH, major 1)" >&2
    exit 2
  fi
  if ! [[ $extra =~ ^[[:space:]a-z0-9_,@=-]*$ ]]; then
    echo "compat: the extra arguments for $tag may hold only dropin flags and names: '$extra'" >&2
    exit 2
  fi
  tags+=("$tag")
  lines+=("$line")
done < "$list"

if [ ${#tags[@]} -eq 0 ]; then
  echo "compat: no released 1.x tag is listed in ${list#"$root"/} yet: nothing to run against HEAD."
  echo "compat: skipped; crates/fil-proto/tests/compat.rs (a simulated later 1.x peer) stands in."
  exit 0
fi

# The bounded set: v1.0.0 if listed, and the newest patch of each of the two newest minors.
selected=()
while IFS= read -r t; do selected+=("$t"); done < <(
  printf '%s\n' "${tags[@]}" | sort -t. -k2,2n -k3,3n | awk -F. '
    { newest[$2 + 0] = $0; if ($0 == "v1.0.0") first = 1 }
    END {
      n = 0; for (m in newest) keys[n++] = m + 0
      for (i = 0; i < n; i++) for (j = i + 1; j < n; j++) if (keys[j] > keys[i]) { t = keys[i]; keys[i] = keys[j]; keys[j] = t }
      if (first) print "v1.0.0"
      for (i = 0; i < n && i < 2; i++) print newest[keys[i]]
    }' | sort -u
)
echo "compat: against HEAD: ${selected[*]} (of ${#tags[@]} listed)"

echo "compat: building HEAD"
cargo build -q --locked --profile ci -p fil-shell --bin fil --example dropin
cargo build -q --locked --profile ci -p fil-daemon --bin fil-daemon-dev
cargo build -q --locked --profile ci -p fil-testbins --bin fwdprobe
head_bin=$target/ci
rm -rf "$work"
mkdir -p "$work"
trees=()
cleanup() {
  for t in ${trees[@]+"${trees[@]}"}; do git worktree remove --force "$t" 2> /dev/null || true; done
}
trap cleanup EXIT
status=0
for tag in "${selected[@]}"; do
  extra=()
  for l in "${lines[@]}"; do
    if [ "${l%% *}" = "$tag" ]; then read -r -a extra <<< "${l#"$tag"}"; fi
  done
  git rev-parse -q --verify "refs/tags/$tag^{commit}" > /dev/null || {
    echo "compat: tag $tag is not in this clone (fetch the tags: a full clone)" >&2
    exit 2
  }
  echo "compat: building $tag"
  tree=$work/src-$tag
  git -c advice.detachedHead=false worktree add -q --detach "$tree" "$tag"
  trees+=("$tree")
  (cd "$tree" && CARGO_TARGET_DIR=$work/target-$tag cargo build -q --locked --profile ci -p fil-shell --bin fil &&
    CARGO_TARGET_DIR=$work/target-$tag cargo build -q --locked --profile ci -p fil-daemon --bin fil-daemon-dev)
  old_bin=$work/target-$tag/ci
  # The drop-in driver takes the daemon from next to --fil: one directory per direction.
  mkdir -p "$work/$tag/old-client" "$work/$tag/new-client"
  cp "$old_bin/fil" "$head_bin/fil-daemon-dev" "$work/$tag/old-client/"
  cp "$head_bin/fil" "$old_bin/fil-daemon-dev" "$work/$tag/new-client/"
  for dir in old-client new-client; do
    if [ "$dir" = old-client ]; then
      echo "compat: $tag client against HEAD's daemon"
    else
      echo "compat: HEAD's client against the $tag daemon"
    fi
    if ! "$head_bin/examples/dropin" --local --set quick --fil "$work/$tag/$dir/fil" \
      --fwdprobe "$head_bin/fwdprobe" ${extra[@]+"${extra[@]}"}; then
      echo "compat: FAILED: $tag, $dir" >&2
      status=1
    fi
  done
done
exit $status
