# The recipes on the release path (docs/m4/release.md section 9): the ones release.yml runs
# (`release-deps`, `notices`, `release-test`, `crates-check`), every `release*` recipe, what the
# release scripts call (`no-test-hooks`), `notices-update`, `reserve-crate-names` and the private
# `_notices` behind the two notices recipes. This file has a code owner (.github/CODEOWNERS); the
# top-level justfile does not, and nothing on the release path reads it: run these as
# `tests/release/just.sh RECIPE` (that is, `just --justfile tests/release/release.just RECIPE`).
# The justfile's recipes of the same names only forward to it, for `just ci` and for convenience;
# the owner's commands (release, release-keys, release-sign-*, release-verify, release-preflight,
# reserve-crate-names) refuse to run through them (entry-check.sh).
#
# Self-contained, which tests/release/recipes.sh enforces: no `import` or `mod` of any kind (every
# file this reads must be owned), `set lazy` (an assignment no recipe uses is never evaluated), and
# the working directory is the repository root, whatever directory just.sh is run from.
set lazy
set working-directory := '../..'

# ---- The release graph ----

# The vt100 emulator backend must stay out of the release binary (only alacritty ships; DESIGN.md
# §6.5). `cargo tree -i` exits 0 even when it prints "nothing to print" to stderr, so test its
# output: it lists vt100's dependents on stdout when vt100 is in the graph.
release-deps:
    #!/usr/bin/env bash
    set -euo pipefail
    out=$(cargo tree -q -p fil-shell -e normal,build --target all -i vt100)
    test -z "$out" || { echo "release-deps: vt100 is in the release build graph of fil" >&2; exit 1; }
    # The test hooks (FIL_TEST_WIRE_REVISION and friends) must not reach the released binary: a
    # production fil must never honor an environment variable that changes what wire it speaks.
    # fil-forward is checked once fil depends on it (until then `cargo tree -i` finds no such package).
    # fil-client's test-hooks carry the prediction guards' seam and the flush tap (which sees what
    # is written to the terminal: docs/m3/plan.md section 4.3).
    for c in fil-proto fil-daemon fil-transport fil-session fil-forward fil-client; do
      if ! tree=$(cargo tree -q -p fil-shell -e normal,build,features --target all -i "$c" 2>&1); then
        case "$tree" in *"did not match any packages"*) continue ;; esac
        echo "$tree" >&2; exit 1
      fi
      # (The install bootstrap is in every build since the owner approved it, M4 OQ-1, 2026-11-04;
      # crates/fil/tests/remote_commands.rs checks what reaches ssh; release-check's `sh -c` scan is
      # a tripwire.)
      hooks=$(grep -E 'feature "test-hooks"' <<<"$tree" || true)
      test -z "$hooks" || { echo "release-deps: $c has a test-only feature in the release build of fil: $hooks" >&2; exit 1; }
    done

# Fails if a binary contains the test hooks' variable names (FIL_TEST_*): such a build reads the
# environment to change what wire it speaks and where its sockets are, and must never be the fil
# that is installed or released. Building an example or a test with `-p fil-shell` unifies the dev-
# dependencies' `test-hooks` features into every target of that invocation, which is why the
# recipes that install `target/release/fil` build the binary in an invocation of its own.
no-test-hooks +binaries:
    #!/usr/bin/env bash
    set -euo pipefail
    for b in {{binaries}}; do
      # A missing binary must fail, not pass: grep reports it as an error (status 2), which an
      # `if grep` would read as "no hooks found".
      [[ -f $b && -r $b ]] || { echo "no-test-hooks: $b is missing or unreadable (build it first)" >&2; exit 1; }
      rc=0
      grep -aq 'FIL_TEST_' "$b" || rc=$?
      case $rc in
        0)
          echo "no-test-hooks: $b was built with the test hooks (FIL_TEST_*); rebuild it with its own 'cargo build --release -p fil-shell --bins'" >&2
          exit 1
          ;;
        1) ;;
        *) echo "no-test-hooks: could not read $b (grep status $rc)" >&2; exit 1 ;;
      esac
    done

# ---- Releases (docs/m4/release.md; the scripts are in tests/release/) ----

# The release scripts' own tests: the workflow lint (permissions, pins, no secrets or caches) with a
# mutation for each rule, tag verification (the signers come from main, never from the tag),
# release-check on synthetic releases (archives and .deb and .rpm packages), the checksums, the crate
# publishing script on fakes, and the packaging templates. Part of `ci`. With FIL_RELEASE_TEST_DOCKER=1
# it also assembles archives and packages twice in the pinned images and requires identical hashes
# (pulls the images).
# The owned checks run first, here and in release.yml's prepare job, so that they hold whatever
# the unowned selftest.sh says: the release workflow's lint, the check workflows' read-only profile
# (they have no code owner) and recipes.sh.
release-test:
    tests/release/lint-workflow.sh .github/workflows/release.yml
    tests/release/lint-workflow.sh --check .github/workflows/linux-check.yml .github/workflows/macos-check.yml
    tests/release/recipes.sh
    tests/release/selftest.sh

# Build the release the way release.yml does: both Linux binaries in the digest-pinned `cross`
# images (needs `cross` 0.2.5 and Docker; on an arm64 host see docs/m4/release.md section 9), the
# universal macOS binary when run on macOS, then the archives, the source archive and SHA256SUMS
# into OUT/files (assembled by the GNU tar and gzip inside the pinned image). Off macOS, name a macOS
# binary with FIL_MACOS_BINARY=path (a dry run's, for example) or the archives are not assembled.
# With the pinned pandoc on PATH (docs/man/build.sh) the archives carry the man pages, completions and
# example configs, as release.yml's do; without it they do not (and `just release-check` then fails on the missing files).
release-build out="target/release-dist":
    #!/usr/bin/env bash
    set -euo pipefail
    out="{{out}}"
    rm -rf "$out"
    for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
      tests/release/build-binary.sh "$t" "$out/bins"
    done
    if [ "$(uname -s)" = Darwin ]; then
      tests/release/macos-build.sh "$out/bins"
    elif [ -n "${FIL_MACOS_BINARY:-}" ]; then
      mkdir -p "$out/bins/universal-apple-darwin"
      cp "$FIL_MACOS_BINARY" "$out/bins/universal-apple-darwin/fil"
    else
      echo "release-build: built the Linux binaries in $out/bins; no macOS binary (set FIL_MACOS_BINARY), so no archives" >&2
      exit 0
    fi
    extra=()
    if ! command -v pandoc > /dev/null; then
      echo "release-build: no docs tree (pandoc 3.13 is not installed; docs/man/install-pandoc.sh does it on Linux): the archives have no man pages or completions" >&2
    elif docs/man/build.sh "$out/docs"; then
      extra=(--extra "$out/docs")
    else
      echo "release-build: docs/man/build.sh failed (see above): the archives have no man pages or completions" >&2
    fi
    tests/release/package.sh --bins "$out/bins" --out "$out/files" ${extra[@]+"${extra[@]}"}
    echo "release-build: the release files are in $out/files; run: just release-check --dry-run $out/files" >&2

# Everything a release candidate must satisfy: `just release-check --dry-run DIR` for a build of a
# branch, `just release-check --tag vX.Y.Z --complete DIR` for a release. One top-level entry per
# archive, the section 4.3 layout, byte-identical bundle copies, the .deb and .rpm packages (exactly
# their archive's files), SHA256SUMS exactly the four archives and the four packages,
# no builder path (/Users/, /home/) or FIL_TEST_ string in a binary, the path remap applied,
# `no-test-hooks` and `release-deps`, static Linux binaries, `fil --version` where it can run, the tag,
# the changelog and the wire. See tests/release/check.sh.
[positional-arguments]
release-check *args:
    tests/release/check.sh "$@"

# The owner's check before signing, and anyone's afterwards: `tests/release/just.sh release-verify 0.1.0`. Tag and
# SHA256SUMS.sig against .github/release-signers on main, the attestations (narrowly), release-check,
# then an independent rebuild of every Linux binary and archive in the Lima VM under amd64 emulation
# (Rosetta), compared hash for hash. `--dry-run --from-run RUN --ref SHA` (SHA: the full 40-hex commit) checks a dry run instead.
# See tests/release/verify.sh and docs/m4/release.md section 9.
[positional-arguments]
release-verify version *args:
    tests/release/entry-check.sh release-verify
    tests/release/verify.sh "$@"

# Everything a release needs, checked before anything is done (docs/m4/release.md section 9.4):
# the tools, disk, a clean HEAD at Gitea's main, the version and changelog, the lockfiles, GitHub's
# main, gh and ssh as the owner, commit signing, the crates-io environment, the tag ruleset, the
# version still free, Docker, the VM (Rosetta), the release and package keys (present, not unlocked)
# and the Developer ID. Read-only; each failure says what it catches and what to do.
# `--from STEP` checks only what STEP and later steps need; `--list` shows the map.
[positional-arguments]
release-preflight version *args:
    tests/release/entry-check.sh release-preflight
    tests/release/release-preflight.sh "$@"

# The one-command release (docs/m4/release.md section 9.5): binds the run to VERSION and HEAD, takes
# the lock, works out from the world (git refs, GitHub's runs and releases, crates.io, the tap) which
# of the seventeen steps are done, runs the preflight for the rest, unlocks the keys, then does every
# step that is left: the mirror commit and the tag (after one typed confirmation); the dry run and
# the suite=all checks, with a waiver of red checks asked at a terminal; the dry run's verification;
# the tag's run and its draft; the verification, the macOS, package and SHA256SUMS signatures, the
# upload and the verification after it; then (after the second confirmation) publish, the
# verification of what is published, the crates-io deployment's approval, the Homebrew tap and its
# install test; and finish (the AUR PKGBUILD, the coordinator's copy, the keys forgotten). Re-run it
# after any failure; `--redo STEP` does a done step again. `--status` is read-only: each step's
# state and the keys.
# See tests/release/release.sh.
[positional-arguments]
release version *args:
    tests/release/entry-check.sh release
    tests/release/release.sh "$@"

# Reserve the crates.io names of the nine published crates with a 0.0.1 placeholder each. Whoever holds the
# publishing account runs it, after `cargo login`; the placeholders are built outside the repository, paced
# for the new-crate limit, and names already registered are skipped (docs/m4/release.md section 10.1).
# Dry run by default; `--publish` really publishes.
[positional-arguments]
reserve-crate-names *args:
    tests/release/entry-check.sh reserve-crate-names
    tests/release/reserve-crate-names.sh "$@"

# The nine published crates package and build for crates.io, in dependency order against a local
# overlay of each other (`cargo publish --workspace --dry-run`); the packaged fil-shell must build.
# Nothing is uploaded and no token is needed (needs network for the crates.io index). Real releases
# are published by release.yml's `crates-io` job, by trusted publishing (N8).
crates-check:
    tests/release/publish-crates.sh --dry-run

# The owner signs the release's two .rpm packages with the package key, on their own machine (never
# CI), in the verified files of `tests/release/just.sh release-verify VERSION --pre-sign`, then rewrites SHA256SUMS
# and writes SHA256SUMS.asc (docs/m4/release.md section 10.4). Needs gpg and rpm (brew install gnupg rpm).
release-sign-packages version *args:
    tests/release/entry-check.sh release-sign-packages
    tests/release/sign-packages.sh "{{version}}" {{args}}

# The owner signs the macOS binary with the Developer ID and notarizes it, on their own Mac (never
# CI; N4): verifies the draft, signs, requires the stripped result to equal CI's, notarizes, and only
# after Apple accepts rebuilds the macOS archive and SHA256SUMS, signs SHA256SUMS with the release
# key and uploads the three files to the DRAFT (docs/m4/release.md section 10.3). Needs Docker, gh
# and the `fil-notary` notarytool profile. Run it after `tests/release/just.sh release-verify VERSION --pre-sign`.
release-sign-macos version *args:
    tests/release/entry-check.sh release-sign-macos
    tests/release/sign-macos.sh "{{version}}" {{args}}

# The release's keys, once per session, on the owner's Mac: `tests/release/just.sh release-keys unlock` puts the release
# key into the ssh-agent with a lifetime, presets the package key's passphrase in gpg-agent (proved by
# a test signature; a reaper forgets it after the TTL) and checks the Developer ID, the notarytool
# profile and a test codesign (options `--ttl 8h`, `--need ssh,gpg,apple`; the one-time gpg-agent setup is
# in docs/m4/release.md section 10.4). Unlock once per release session; `status` and `forget` too.
[positional-arguments]
release-keys *args:
    tests/release/entry-check.sh release-keys
    tests/release/release-keys.sh "$@"

# ---- Notices (docs/m4/release.md section 7) ----

# The cargo-about that renders THIRD-PARTY-NOTICES.md. Pinned: another version can render other
# bytes, and `notices` fails on it. The workflows install exactly this version (install-action);
# locally: cargo install cargo-about --locked --version 0.9.3 --features cli
about_version := "0.9.3"

# THIRD-PARTY-NOTICES.md is current: re-rendered from about.hbs and about.toml for the release graph
# (`-p fil-shell`, default features, normal and build edges, the four release targets of about.toml), it
# is byte for byte the committed file. Fails if a crate in that graph has a license outside the
# allowed list, if about.toml's list is not deny.toml's, if cargo-about warns (a clarify checksum
# that no longer matches) or falls back to a template text ("<year> <copyright holders>"), if a
# crate's NOTICE file is not reproduced, or if the notices miss a crate of `cargo tree`'s graph for
# the same targets. Part of `ci`; needs cargo-about (above) and python3.
notices: (_notices "check")

# Rewrite THIRD-PARTY-NOTICES.md after a dependency change (then review the diff and commit it).
notices-update: (_notices "update")

_notices mode:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{source_directory()}}/../.."
    want={{about_version}}
    have=$(cargo about --version 2>/dev/null | awk '{ print $2 }' || true)
    if [[ "$have" != "$want" ]]; then
      echo "notices: needs cargo-about $want (found '${have:-none}'): cargo install cargo-about --locked --version $want --features cli" >&2
      exit 1
    fi
    # The quoted strings of a TOML array `KEY = [ ... ]` in FILE, sorted.
    list() { awk -v key="$2" '$0 ~ "^"key" = \\[" { on = 1; next } on && /^\]/ { exit } on' "$1" | sed 's/#.*//' | grep -o '"[^"]*"' | tr -d '"' | LC_ALL=C sort; }
    # The allowed licenses: about.toml's `accepted` must be deny.toml's `[licenses] allow`, as sets.
    if [[ -z "$(list deny.toml allow)" ]]; then echo "notices: found no allow list in deny.toml" >&2; exit 1; fi
    if ! diff <(list deny.toml allow) <(list about.toml accepted) > /dev/null; then
      echo "notices: about.toml's accepted list differs from deny.toml's allow list (< deny.toml, > about.toml):" >&2
      diff <(list deny.toml allow) <(list about.toml accepted) >&2 || true
      exit 1
    fi
    targets=$(list about.toml targets)
    [[ -n "$targets" ]] || { echo "notices: about.toml lists no targets" >&2; exit 1; }
    scratch=$(mktemp -d)
    trap 'rm -rf "$scratch"' EXIT
    # cargo-about runs offline (--frozen), so that no license text comes from the network and the
    # output depends only on the lock file and the crates' own files; fetch the crates first.
    cargo fetch --locked -q
    # --fail: a crate whose license is outside the list, or cannot be read, fails the render.
    if ! cargo about generate --frozen --fail -m crates/fil/Cargo.toml -c about.toml about.hbs -o "$scratch/raw.md" 2> "$scratch/about.log"; then
      cat "$scratch/about.log" >&2
      echo "notices: cargo-about failed: a crate in the release graph has a license outside the allowed list (deny.toml, about.toml), or one it cannot read" >&2
      exit 1
    fi
    # cargo-about only warns when a clarification's checksum no longer matches, and then falls back.
    if grep -qE 'WARN|ERROR' "$scratch/about.log"; then
      cat "$scratch/about.log" >&2
      echo "notices: cargo-about warned (a [CRATE.clarify] in about.toml that no longer matches its file?); fix it before rendering" >&2
      exit 1
    fi
    # The workspace's published crates are in cargo-about's data (about.toml, [private]); about.hbs
    # leaves out each crate and license entry without a registry source. That is exact only if no
    # license entry, and no license kind, holds both workspace crates and registry crates.
    cargo about generate --frozen --format json -m crates/fil/Cargo.toml -c about.toml -o "$scratch/about.json" 2> "$scratch/about-json.log" \
      || { cat "$scratch/about-json.log" >&2; echo "notices: cargo-about's JSON render failed" >&2; exit 1; }
    python3 -I tests/release/notice_workspace.py "$scratch/about.json"
    # A license file it does not recognize gets the canonical text, with its template fields.
    if grep -nE '<year>|<copyright holders>|<owner>|<name of author>' "$scratch/raw.md" > "$scratch/placeholders"; then
      echo "notices: a license text is a template, not the crate's own notice (add a [CRATE.clarify] to about.toml):" >&2
      cat "$scratch/placeholders" >&2
      exit 1
    fi
    # The ISC crates' per-file copyright lines, and the NOTICE-file check (tests/release/notice_scan.py).
    cargo metadata --locked --offline --format-version 1 --manifest-path crates/fil/Cargo.toml > "$scratch/meta.json"
    tr -d '\r' < "$scratch/raw.md" > "$scratch/raw-lf.md"
    python3 -I tests/release/notice_scan.py "$scratch/raw-lf.md" "$scratch/meta.json" > "$scratch/source-notices.md"
    toolchain=$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)
    [[ -n "$toolchain" ]] || { echo "notices: no channel in rust-toolchain.toml" >&2; exit 1; }
    sed "s/@RUST_TOOLCHAIN@/$toolchain/g" "$scratch/raw-lf.md" \
      | awk -v f="$scratch/source-notices.md" '$0 == "@SOURCE_NOTICES@" { while ((getline l < f) > 0) print l; next } { print }' > "$scratch/notices.md"
    if grep -nE '@[A-Z][A-Z_]+@' "$scratch/notices.md" >&2; then echo "notices: a placeholder was not filled in" >&2; exit 1; fi
    # Nothing of this machine may reach the file: it ships in every archive.
    for p in "$PWD" "${CARGO_HOME:-$HOME/.cargo}" "$HOME"; do
      if grep -qF -- "$p/" "$scratch/notices.md"; then echo "notices: the rendered file contains a local path ($p)" >&2; exit 1; fi
    done
    # Coverage: the crate table is exactly the registry crates of the release graph `cargo tree` sees.
    sed -n '/^| Crate | Version | License |$/,/^$/p' "$scratch/notices.md" | tail -n +3 | awk -F' [|] ' 'NF >= 3 { sub(/^[|] /, "", $1); print $1 " v" $2 }' | LC_ALL=C sort -u > "$scratch/listed"
    tflags=$(for t in $targets; do printf -- '--target %s ' "$t"; done)
    # shellcheck disable=SC2086
    cargo tree -q --locked -p fil-shell -e normal,build $tflags --prefix none --format '{p}' | grep -v ' (/' | sed -E 's/( \((\*|proc-macro)\))+$//' | sed '/^$/d' | LC_ALL=C sort -u > "$scratch/graph"
    if [[ ! -s "$scratch/graph" ]] || ! diff "$scratch/graph" "$scratch/listed" > "$scratch/cover.diff"; then
      echo "notices: the notices' crates are not the release graph's (< cargo tree, > notices):" >&2
      cat "$scratch/cover.diff" >&2
      exit 1
    fi
    if [[ "{{mode}}" == update ]]; then
      cp "$scratch/notices.md" THIRD-PARTY-NOTICES.md
      echo "notices: wrote THIRD-PARTY-NOTICES.md ($(wc -l < "$scratch/listed" | tr -d ' ') crates); review the diff and commit it" >&2
    elif cmp -s "$scratch/notices.md" THIRD-PARTY-NOTICES.md; then
      echo "notices: THIRD-PARTY-NOTICES.md is current ($(wc -l < "$scratch/listed" | tr -d ' ') crates)" >&2
    else
      echo "notices: THIRD-PARTY-NOTICES.md is stale or edited by hand; run 'just notices-update' and commit it. The difference:" >&2
      diff -u -L THIRD-PARTY-NOTICES.md -L rendered THIRD-PARTY-NOTICES.md "$scratch/notices.md" | head -n 40 >&2 || true
      exit 1
    fi
