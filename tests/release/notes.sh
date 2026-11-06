#!/usr/bin/env bash
# The body of the draft release: the changelog section of the version (if CHANGELOG.md has one) and
# how to verify what was downloaded. Printed to stdout.
#
#   notes.sh VERSION [OWNER/REPO]
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

v=${1:?usage: notes.sh VERSION [OWNER/REPO]}
repo=${2:-anthonytw/fil}
vre=$(printf '%s' "$v" | sed 's/[.]/[.]/g')

if [ -f "$ROOT/CHANGELOG.md" ]; then
  awk -v re="^##[[:space:]]+\\\\[?v?$vre\\\\]?([[:space:]]|\$)" '
    $0 ~ re { p = 1; print; next }
    p && /^##[[:space:]]/ { exit }
    p { print }
  ' "$ROOT/CHANGELOG.md"
fi
# The macOS paragraph below is true of the published release: the owner signs and notarizes the macOS
# binary after CI (`just release-sign-macos`, docs/m4/release.md section 10.3), before publishing.
cat << EOT

## Verify what you downloaded

Download the archive for your system, \`SHA256SUMS\` and \`SHA256SUMS.sig\`, then:

\`\`\`sh
sha256sum -c SHA256SUMS --ignore-missing      # or: shasum -a 256 -c SHA256SUMS --ignore-missing
gh attestation verify fil-$v-x86_64-linux-musl.tar.gz --repo $repo \\
  --signer-workflow $repo/.github/workflows/release.yml --source-ref refs/tags/v$v
# the signature: SIGNERS is .github/release-signers as it is on main (never the tag's copy)
ssh-keygen -Y verify -f SIGNERS -I release -n fil-release -s SHA256SUMS.sig < SHA256SUMS
\`\`\`

\`SHA256SUMS.sig\` is an SSH signature by the release key listed in \`.github/release-signers\`
**on main**. fil is GPL-3.0-or-later: each archive carries \`share/doc/fil/LICENSE\` and the
third-party notices, \`share/doc/fil/THIRD-PARTY-NOTICES.md\`. \`fil-$v-source.tar.gz\` is the
Corresponding Source with every dependency vendored; it builds offline. The Linux binaries and
archives are reproducible (\`just release-verify\` rebuilds them in a pinned image).

**macOS.** Download \`fil-$v-universal-apple-darwin.tar.gz\`: its \`bin/fil\` is signed with the
owner's Developer ID (hardened runtime) and notarized by Apple, which a bare command-line binary
cannot staple, so macOS asks Apple online the first time it runs. The release also holds
\`fil-$v-universal-apple-darwin.ci.tar.gz\`: CI's own build, ad hoc signed and **not notarized**, kept
only so that the signed archive can be checked. Do not install it. GitHub attests that one (CI never
saw the signed archive), and the signed archive is checked against it, from a checkout of the tag
and in the directory you downloaded to (this needs python3, nothing else):

\`\`\`sh
gh attestation verify fil-$v-universal-apple-darwin.ci.tar.gz --repo $repo \\
  --signer-workflow $repo/.github/workflows/release.yml --source-ref refs/tags/v$v
# the signed archive is that archive with only the signature of bin/fil changed
tests/release/verify-macos.sh agree . $v
\`\`\`

\`just release-verify $v\` does all of this, and more (it also rebuilds the other archives). The macOS
binary itself is not independently reproduced from source by CI; \`--mac-rebuild FILE\` compares a
build of your own with it once both signatures are normalized away.
EOT
