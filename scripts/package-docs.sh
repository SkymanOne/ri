#!/bin/sh
# Packages the documentation the model reads as yapi-docs.tar.gz in <out-dir>,
# with yapi-docs.tar.gz.sha256 in sha256sum's format. The archive holds this
# repository's docs/*.md at the top level, the pi release that
# tests/fixtures/pi/generator pins under pi/, and .version, the yapi version
# it documents. pi's part is the text of README.md, docs and examples from
# the npm package, checked against the lock file's integrity, without
# examples/plugins (an experimental plugin yapi does not run), and pi's
# LICENSE. install.sh and yapi itself unpack the archive into
# <agent dir>/docs.
#
# Usage: scripts/package-docs.sh <out-dir>
# Needs curl, openssl, python3 and sha256sum.
set -eu

if [ $# -ne 1 ]; then
    echo "Usage: $0 <out-dir>" >&2
    exit 1
fi
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$1"
out=$(cd "$1" && pwd)
# The SHA-256 of pi's LICENSE at the pinned release.
license_sha256=0457f5bcec3b3b211605dfb5d1a49042fd638f3686a410fe099c24a25af13c48

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
read -r pi_version tarball integrity <<EOF
$(python3 -c '
import json, sys
lock = json.load(open(sys.argv[1]))
pi = lock["packages"]["node_modules/@earendil-works/pi-coding-agent"]
print(pi["version"], pi["resolved"], pi["integrity"])
' "$root/tests/fixtures/pi/generator/package-lock.json")
EOF

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -fsSL -o "$work/pi.tgz" "$tarball"
actual="sha512-$(openssl dgst -sha512 -binary "$work/pi.tgz" | openssl base64 -A)"
if [ "$actual" != "$integrity" ]; then
    echo "$tarball does not match its integrity $integrity" >&2
    exit 1
fi
curl -fsSL -o "$work/LICENSE" "https://raw.githubusercontent.com/earendil-works/pi/v$pi_version/LICENSE"
if ! echo "$license_sha256  $work/LICENSE" | sha256sum -c - >/dev/null; then
    echo "pi's LICENSE at v$pi_version is not the one this script pins" >&2
    exit 1
fi

docs="$work/docs"
mkdir -p "$docs/pi" "$work/npm"
cp "$root"/docs/*.md "$docs/"
echo "$version" >"$docs/.version"
cp "$work/LICENSE" "$docs/pi/LICENSE"
tar -xzf "$work/pi.tgz" -C "$work/npm"
(
    cd "$work/npm/package"
    find README.md docs examples -type f ! -path 'examples/plugins/*' | while read -r file; do
        # Text only: images and other binaries stay out.
        if grep -Iq . "$file"; then
            mkdir -p "$docs/pi/$(dirname "$file")"
            cp "$file" "$docs/pi/$file"
        fi
    done
)
# macOS tar would otherwise add AppleDouble files for extended attributes.
(cd "$docs" && COPYFILE_DISABLE=1 tar -czf "$out/yapi-docs.tar.gz" .version *.md pi)

cd "$out"
sha256sum yapi-docs.tar.gz >yapi-docs.tar.gz.sha256
echo "$out/yapi-docs.tar.gz"
