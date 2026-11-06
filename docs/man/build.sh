#!/usr/bin/env bash
# Writes the generated documentation files into OUT, laid out like a release archive
# (docs/m4/release.md section 4.3):
#
#   share/man/man1/fil.1                                  clap_mangen, from the command line
#   share/man/man5/fil-config.5, fil-policy.5             pandoc, from docs/user/config.md and policy.md
#   share/bash-completion/completions/fil, share/zsh/site-functions/_fil,
#   share/fish/vendor_completions.d/fil.fish              clap_complete
#   share/doc/fil/{config,daemon,policy}.example.toml     docs/user/examples/
#
#   docs/man/build.sh OUT
#
# `just docs` runs it, and release.yml's package job does, then hands OUT to package.sh --extra.
# Pandoc is a build tool, not a dependency of any binary, and its version is pinned: another
# version can write other bytes, and the archives must be reproducible. FIL_DOCS_ANY_PANDOC=1
# accepts another version for a local look (never for a release).
set -euo pipefail

PANDOC_VERSION=3.13

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
out=${1:?usage: docs/man/build.sh OUT}
rm -rf "$out"
mkdir -p "$out"
out=$(cd "$out" && pwd)

cd "$root"
cargo run --quiet --locked -p fil-shell --example gen_docs -- --out "$out"

# The committed fil.1 is what the generator writes (tests/docs_sync.rs checks it too).
cmp -s "$out/share/man/man1/fil.1" docs/man/fil.1 \
  || { echo "docs: docs/man/fil.1 is out of date: run 'just docs-man'" >&2; exit 1; }

command -v pandoc > /dev/null || { echo "docs: pandoc $PANDOC_VERSION is needed for fil-config(5) and fil-policy(5)" >&2; exit 1; }
have=$(pandoc --version | sed -n '1s/^pandoc //p')
if [ "$have" != "$PANDOC_VERSION" ] && [ "${FIL_DOCS_ANY_PANDOC:-}" != 1 ]; then
  echo "docs: pandoc $have found, $PANDOC_VERSION is pinned (FIL_DOCS_ANY_PANDOC=1 for a local look)" >&2
  exit 1
fi

# One man page from one guide page. No date, so the bytes do not depend on when it ran; the
# relative links of the guide are rewritten by the filter. A NAME section goes first, in the
# form whatis and apropos read: "name - description".
mkdir -p "$out/share/man/man5"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
man5() { # NAME DESCRIPTION SOURCE
  printf '# NAME\n\n%s - %s\n\n' "$1" "$2" > "$scratch/$1.name.md"
  pandoc --from gfm --to man --standalone \
    --lua-filter docs/man/strip-links.lua \
    --metadata "title=$1" --metadata section=5 --metadata date= \
    --metadata footer=fil --metadata "header=fil manual" \
    --output "$out/share/man/man5/$1.5" "$scratch/$1.name.md" "$3"
}
man5 fil-policy "the fil server policy file and its mirror of the sshd forwarding settings" docs/user/policy.md
man5 fil-config "the fil client configuration file" docs/user/config.md

# The examples ship in the archive's doc directory.
mkdir -p "$out/share/doc/fil"
for kind in config daemon policy; do
  install -m 644 "docs/user/examples/$kind.toml" "$out/share/doc/fil/$kind.example.toml"
done

# Modes: the archive's files are 644 whatever the umask.
find "$out" -type f -exec chmod 644 {} +
