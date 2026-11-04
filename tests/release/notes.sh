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
cat << EOT

## Verify what you downloaded

Download the archive for your system, \`SHA256SUMS\` and \`SHA256SUMS.sig\`, then:

\`\`\`sh
sha256sum -c SHA256SUMS --ignore-missing      # or: shasum -a 256 -c SHA256SUMS --ignore-missing
gh attestation verify fil-$v-x86_64-linux-musl.tar.gz --repo $repo \\
  --signer-workflow $repo/.github/workflows/release.yml --source-ref refs/tags/v$v
\`\`\`

\`SHA256SUMS.sig\` is an SSH signature (\`ssh-keygen -Y verify -n fil-release\`) by the release key
listed in \`.github/release-signers\` **on main**. \`fil-$v-source.tar.gz\` is the Corresponding Source
with every dependency vendored; it builds offline.
EOT
