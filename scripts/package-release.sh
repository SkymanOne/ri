#!/bin/sh
# Packages a release build as installers expect it: yapi-<target>.tar.gz
# holding the yapi binary, and yapi-<target>.tar.gz.sha256 in sha256sum's
# format. The archive also holds LICENSE-MIT, LICENSE-APACHE and <notices>,
# which scripts/third-party-notices.sh writes, as THIRD-PARTY-NOTICES. The
# names carry no version, so the latest release's
# archive is always at releases/latest/download/yapi-<target>.tar.gz. install.sh
# and the cargo-binstall metadata in crates/yapi/Cargo.toml rely on these names
# and take only yapi from the archive.
#
# Usage: scripts/package-release.sh <binary> <target> <out-dir> <notices>
set -eu

if [ $# -ne 4 ]; then
    echo "Usage: $0 <binary> <target> <out-dir> <notices>" >&2
    exit 1
fi
binary=$1
name="yapi-$2"
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$3"
out=$(cd "$3" && pwd)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp "$binary" "$work/yapi"
chmod 755 "$work/yapi"
cp "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$work/"
cp "$4" "$work/THIRD-PARTY-NOTICES"
files="yapi LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES"
# macOS tar would otherwise add AppleDouble files for extended attributes.
(cd "$work" && COPYFILE_DISABLE=1 tar -czf "$out/$name.tar.gz" $files)

cd "$out"
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$name.tar.gz" >"$name.tar.gz.sha256"
else
    shasum -a 256 "$name.tar.gz" >"$name.tar.gz.sha256"
fi
echo "$out/$name.tar.gz"
