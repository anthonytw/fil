#!/usr/bin/env bash
# Old against new within 1.x (docs/v1/wire.md, DESIGN.md section 8.5 "Compatibility"): for every
# tag in tests/compat/tags.txt, build that tag's `fil` and `fil-daemon-dev` and run the drop-in
# quick set against a private daemon twice: the old `fil` with HEAD's daemon, then HEAD's `fil`
# with the old daemon. linux-check's `compat` job runs it (suites extended and all); it needs the
# tags fetched (a full clone). With no tag listed it says so and passes.
#
#   tests/compat/run.sh [TAGS_FILE]
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
list=${1:-$root/tests/compat/tags.txt}
target=${CARGO_TARGET_DIR:-$root/target}
work=$target/compat

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
  lines+=("$line")
done < "$list"

if [ ${#lines[@]} -eq 0 ]; then
  echo "compat: no released 1.x tag is listed in ${list#"$root"/} yet: nothing to run against HEAD."
  echo "compat: skipped; crates/fil-proto/tests/compat.rs (a simulated later 1.x peer) stands in."
  exit 0
fi

cd "$root"
echo "compat: building HEAD"
cargo build -q --profile ci -p fil-shell --bin fil --example dropin
cargo build -q --profile ci -p fil-daemon --bin fil-daemon-dev
cargo build -q --profile ci -p fil-testbins --bin fwdprobe
head_bin=$target/ci
rm -rf "$work"
mkdir -p "$work"
status=0
for line in "${lines[@]}"; do
  tag=${line%% *}
  read -r -a extra <<< "${line#"$tag"}"
  git rev-parse -q --verify "refs/tags/$tag^{commit}" > /dev/null || {
    echo "compat: tag $tag is not in this clone (fetch the tags: a full clone)" >&2
    exit 2
  }
  echo "compat: building $tag"
  tree=$work/src-$tag
  git -c advice.detachedHead=false worktree add -q --detach "$tree" "$tag"
  (cd "$tree" && CARGO_TARGET_DIR=$work/target-$tag cargo build -q --profile ci -p fil-shell --bin fil &&
    CARGO_TARGET_DIR=$work/target-$tag cargo build -q --profile ci -p fil-daemon --bin fil-daemon-dev)
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
  git worktree remove --force "$tree"
done
exit $status
