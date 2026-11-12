#!/usr/bin/env bash
# The release path reads tests/release/release.just (owned), never the top-level justfile (unowned).
#
#   recipes.sh [ROOT]     ROOT: the repository (default: this script's)
#
# The justfile has no code owner (.github/CODEOWNERS), and just evaluates a justfile's assignments
# (a backtick, shell()) and the files it imports or mounts as soon as it parses it, before any
# recipe runs. So the release path never parses it: release.yml, the release scripts and the owner
# run release.just alone, through tests/release/just.sh. This fails unless:
#   1. release.just parses on its own and defines exactly the recipes of RECIPES below (so the list
#      cannot go stale), with `set lazy` and the working directory `../..` (the repository root);
#   2. release.just imports and mounts nothing: no `import`, `import?`, `mod` or `mod?` line, and no
#      module in its dump (every file it reads is release.just itself);
#   3. each of the owner's commands (OWNER below) runs entry-check.sh first, which refuses to run
#      under any other justfile;
#   4. no release script (tests/release/*.sh but the self-test) runs `just` itself: they run
#      tests/release/just.sh. (release.yml's run: lines are lint-workflow.sh's rule 9.)
# It reads `just --dump --dump-format json` of release.just (just's own parser) and greps the
# scripts; it runs nothing else. release.yml's prepare job runs it, and so do
# `tests/release/just.sh release-preflight` (its `recipes` check) and `release-test`.
set -euo pipefail
ROOT=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}
REL=tests/release/release.just
# The release path: what release.yml runs, every release* recipe, what the release scripts call, and
# the private recipe behind `notices`.
RECIPES="_notices crates-check no-test-hooks notices notices-update release release-build release-check
release-deps release-keys release-preflight release-sign-macos release-sign-packages release-test
release-verify reserve-crate-names"
# The owner's commands: they unlock or use the keys, or decide whether a release is good.
OWNER="release release-keys release-preflight release-sign-macos release-sign-packages release-verify
reserve-crate-names"

die() { echo "recipes: $*" >&2; exit 1; }
command -v just > /dev/null || die "needs just"
command -v python3 > /dev/null || die "needs python3"
[ -f "$ROOT/$REL" ] || die "no $REL in $ROOT"
for f in just.sh entry-check.sh; do
  [ -x "$ROOT/tests/release/$f" ] || die "no executable tests/release/$f in $ROOT"
done
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# 2, by the text: just parses these only at the top level of a file, unindented.
if grep -nE '^(import|mod)([?[:space:]]|$)' "$ROOT/$REL" > "$work/imports"; then
  sed "s|^|$REL:|" "$work/imports" >&2
  die "$REL imports or mounts another file (above): keep it self-contained, every file the release path reads must have a code owner"
fi
# --justfile alone: the working-directory setting is relative to release.just's directory.
just --justfile "$ROOT/$REL" --dump --dump-format json > "$work/alone.json" 2> "$work/err" \
  || { cat "$work/err" >&2; die "$REL does not parse on its own"; }

python3 -I - "$work/alone.json" "$REL" "$RECIPES" "$OWNER" << 'EOF'
import json, sys
d = json.load(open(sys.argv[1]))
rel, want, owner = sys.argv[2], sorted(sys.argv[3].split()), sys.argv[4].split()
bad = []
have = sorted(d["recipes"])
if have != want:
    bad.append(f"{rel} defines {have}, not exactly the release path {want} (tests/release/recipes.sh, RECIPES)")
if d.get("modules"):
    bad.append(f"{rel} mounts modules {sorted(d['modules'])}: keep it self-contained")
s = d["settings"]
if s.get("lazy") is not True:
    bad.append(f"{rel} lacks `set lazy`")
if s.get("working_directory") != "../..":
    bad.append(f"{rel}'s working directory is {s.get('working_directory')!r}, not '../..' (the repository root)")
def first_line(r):
    body = r.get("body") or []
    if not body:
        return ""
    # A body line is a list of fragments: text is a string, an interpolation a list.
    return "".join(f if isinstance(f, str) else "\0" for f in body[0])
for r in owner:
    rec = d["recipes"].get(r)
    if rec is None:
        continue
    if first_line(rec) != f"tests/release/entry-check.sh {r}":
        bad.append(f"{r} does not run `tests/release/entry-check.sh {r}` first")
for b in bad:
    print(f"recipes: {b}", file=sys.stderr)
sys.exit(1 if bad else 0)
EOF

# 4: a command that is `just` (at the start of a line, or after ;, &&, ||, |, (, $(, `then`, `do`,
# `exec`...), once the quoted strings are taken out: a message that names a command is quoted.
scripts=()
for f in "$ROOT"/tests/release/*.sh; do
  case ${f##*/} in selftest.sh | just.sh | recipes.sh) ;; *) scripts+=("$f") ;; esac
done
python3 -I - "${scripts[@]}" << 'EOF' || die "a release script runs just (above), which reads the unowned justfile: run \"\$ROOT/tests/release/just.sh\" instead"
import re, sys
cmd = re.compile(r"(^|[;&|(]|\$\(|\b(?:then|do|else|exec|env|time|command)\s)\s*just(\s|$)")
bad = False
for p in sys.argv[1:]:
    for n, line in enumerate(open(p), 1):
        code = re.sub(r'"(?:[^"\\]|\\.)*"|\'[^\']*\'', '""', line.split(" #")[0])
        if code.lstrip().startswith("#"):
            continue
        if cmd.search(code):
            print(f"tests/release/{p.rsplit('/', 1)[-1]}:{n}: {line.rstrip()}", file=sys.stderr)
            bad = True
sys.exit(1 if bad else 0)
EOF
echo "recipes: the release path's $(wc -w <<< "$RECIPES" | tr -d ' ') recipes are $REL's, which is self-contained, and no release script reads the justfile"
