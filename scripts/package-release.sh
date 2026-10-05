#!/bin/sh
# Packages a release build as installers expect it: yapi-<target>.tar.gz
# holding only the yapi binary, and yapi-<target>.tar.gz.sha256 in sha256sum's
# format. The names carry no version, so the latest release's archive is always
# at releases/latest/download/yapi-<target>.tar.gz. install.sh and the
# cargo-binstall metadata in crates/yapi/Cargo.toml rely on these names.
#
# Usage: scripts/package-release.sh <binary> <target> <out-dir>
set -eu

if [ $# -ne 3 ]; then
    echo "Usage: $0 <binary> <target> <out-dir>" >&2
    exit 1
fi
binary=$1
name="yapi-$2"
mkdir -p "$3"
out=$(cd "$3" && pwd)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp "$binary" "$work/yapi"
chmod 755 "$work/yapi"
# macOS tar would otherwise add AppleDouble files for extended attributes.
(cd "$work" && COPYFILE_DISABLE=1 tar -czf "$out/$name.tar.gz" yapi)

cd "$out"
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$name.tar.gz" >"$name.tar.gz.sha256"
else
    shasum -a 256 "$name.tar.gz" >"$name.tar.gz.sha256"
fi
echo "$out/$name.tar.gz"
