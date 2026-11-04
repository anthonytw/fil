#!/usr/bin/env bash
# The nine published crates on crates.io (docs/m4/release.md section 10.1; docs/m4/packaging.md).
#
#   publish-crates.sh --dry-run      package and verify every crate in dependency order, against a
#                                    local overlay of the others (`cargo publish --workspace
#                                    --dry-run`); nothing is uploaded and no token is needed. The
#                                    packaged fil-shell must build, and every crate must carry the
#                                    GPL text (LICENSE, a link to the root one, which cargo packages
#                                    as a file). `just crates-check` runs this.
#   publish-crates.sh --publish      publish them, one at a time in dependency order; a crate whose
#                                    version is already on crates.io is skipped, so a run that
#                                    stopped halfway can be run again. Only release.yml's `crates-io`
#                                    job runs this, with the short-lived token of trusted publishing
#                                    in CARGO_REGISTRY_TOKEN (N8): there is no long-lived token.
#                                    It compiles NOTHING (`--no-verify`): no build script or proc
#                                    macro of any crate runs beside the token. The same commit was
#                                    verified by --dry-run in release.yml's prepare job, and the job
#                                    checks that the tag still names that commit (prepare.sh).
#   publish-crates.sh --list         print the nine crates in dependency order
#
# The crates are the workspace members whose `publish` is ["crates-io"]; selftest.sh checks that they
# are exactly the nine names reserved by reserve-crate-names.sh. The order comes from cargo metadata
# (normal dependencies between them), never from a list kept by hand.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

mode=${1:-}
case $mode in --dry-run | --publish | --list) ;; *) die "usage: publish-crates.sh --dry-run | --publish | --list" ;; esac
cargo=${FIL_PUBLISH_CARGO:-cargo}
curl=${FIL_PUBLISH_CURL:-curl}
cd "$ROOT"

order=$(cargo metadata --locked --offline --no-deps --format-version 1 | python3 -I -c '
import json, sys
m = json.load(sys.stdin)
pub = {p["name"]: p for p in m["packages"] if p["publish"] == ["crates-io"]}
deps = {n: sorted(d["name"] for d in p["dependencies"] if d["name"] in pub and d["kind"] is None) for n, p in pub.items()}
done, out = set(), []
while len(out) < len(pub):
    ready = sorted(n for n in pub if n not in done and all(d in done for d in deps[n]))
    if not ready:
        sys.exit("publish-crates: the published crates depend on each other in a cycle")
    for n in ready:
        done.add(n)
        out.append(n)
print(" ".join(out))
')
[ -n "$order" ] || die "no crate with publish = [\"crates-io\"]"

case $mode in
  --list)
    echo "$order"
    ;;
  --dry-run)
    log "packaging and verifying, in order: $order"
    for c in $order; do
      "$cargo" package --list --locked --allow-dirty -p "$c" 2> /dev/null | grep -qx LICENSE || die "$c's package has no LICENSE (link crates/<dir>/LICENSE to ../../LICENSE)"
    done
    log "every crate's package carries LICENSE"
    # A target directory of its own, emptied first. cargo treats a registry crate's build as done
    # once its name and version have been built, and the overlay's crates are always the workspace's
    # version: in a shared target directory a crate changed since an earlier dry run (a merge
    # changed fil-predict at 0.0.0) was not rebuilt, and fil-client was verified against the old one.
    tdir=${CARGO_TARGET_DIR:-$ROOT/target}/crates-check
    rm -rf "$tdir"
    CARGO_TARGET_DIR=$tdir "$cargo" publish --workspace --dry-run --locked
    ;;
  --publish)
    [ -n "${CARGO_REGISTRY_TOKEN:-}" ] || die "no CARGO_REGISTRY_TOKEN (release.yml's crates-io job gets one by trusted publishing)"
    V=$(cargo_version)
    for c in $order; do
      code=$("$curl" -s -o /dev/null -w '%{http_code}' -A 'fil-release (https://github.com/anthonytw/fil)' "https://crates.io/api/v1/crates/$c/$V")
      case $code in
        200) log "$c $V is already on crates.io: skipped" ;;
        404)
          log "publishing $c $V"
          "$cargo" publish --locked --no-verify -p "$c"
          ;;
        *) die "crates.io answered $code for $c $V; stopping (run again to resume)" ;;
      esac
    done
    log "published, in order: $order"
    ;;
esac
