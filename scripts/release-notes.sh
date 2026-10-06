#!/bin/sh
# Prints the section of CHANGELOG.md for <version>, without its heading: the
# notes of the GitHub release. Fails when CHANGELOG.md has no such section.
#
# Usage: scripts/release-notes.sh <version>   (as in Cargo.toml, without a v)
set -eu

if [ $# -ne 1 ]; then
    echo "Usage: $0 <version>" >&2
    exit 1
fi
root=$(cd "$(dirname "$0")/.." && pwd)
notes=$(awk -v heading="## [$1]" '
    /^## / { if (found) exit; found = index($0, heading) == 1; next }
    found
' "$root/CHANGELOG.md")
if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
    echo "CHANGELOG.md has no section for $1" >&2
    exit 1
fi
printf '%s\n' "$notes"
