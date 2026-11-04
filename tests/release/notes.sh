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
# TODO (docs/m4/release.md section 10.3, the notarization work item): v1 macOS binaries are signed and
# notarized by `just release-sign-macos`; when that lands, replace the "ad-hoc signed ... not notarized"
# sentence below. Until then it is true of what CI builds, so it stays.
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
archives are reproducible (\`just release-verify\` rebuilds them in a pinned image). The macOS
binary is **ad-hoc signed** (no Developer ID, not notarized) and is not independently reproduced;
the macOS archive is checked only around that binary.
EOT
