#!/bin/sh
# Installs yapi from a GitHub release: downloads the binary for this system,
# checks it against its published SHA-256 and copies it into a directory on
# PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/SkymanOne/ri/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/SkymanOne/ri/main/install.sh | sh -s -- --version v0.1.0 --to /usr/local/bin
#
# Options, or the environment variables that set them:
#   --version <tag>   YAPI_VERSION       release to install (default: the latest)
#   --to <dir>        YAPI_INSTALL_DIR   where to put yapi (default: ~/.local/bin)
#   YAPI_RELEASES_URL                    the releases page (default: GitHub's)
set -eu

releases="${YAPI_RELEASES_URL:-https://github.com/SkymanOne/ri/releases}"
version="${YAPI_VERSION:-latest}"
dir="${YAPI_INSTALL_DIR:-${HOME:-}/.local/bin}"
from_source="cargo install --locked --git https://github.com/SkymanOne/ri yapi"

fail() {
    echo "install.sh: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || fail "--version needs a value"
            version=$2
            shift 2
            ;;
        --to)
            [ $# -ge 2 ] || fail "--to needs a value"
            dir=$2
            shift 2
            ;;
        -h | --help)
            cat <<EOF
Usage: install.sh [--version <tag>] [--to <dir>]

Installs yapi from a GitHub release, checked against its SHA-256.

  --version <tag>   release to install (default: the latest), or YAPI_VERSION
  --to <dir>        where to put yapi (default: ~/.local/bin), or YAPI_INSTALL_DIR
EOF
            exit 0
            ;;
        *) fail "unknown option $1" ;;
    esac
done

command -v curl >/dev/null 2>&1 || fail "needs curl"
command -v tar >/dev/null 2>&1 || fail "needs tar"
if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    fail "needs sha256sum or shasum to check the download"
fi

# The release target for this system.
os=$(uname -s)
arch=$(uname -m)
case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) fail "no release build for $arch. Build from source: $from_source" ;;
esac
case "$os" in
    Linux)
        if ldd --version 2>&1 | grep -qi musl; then
            fail "release builds need glibc, and this system uses musl. Build from source: $from_source"
        fi
        glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | sed 's/^glibc //')
        case "$glibc" in
            2.[0-9] | 2.[0-9].* | 2.[12][0-9] | 2.[12][0-9].* | 2.3[0-4] | 2.3[0-4].*)
                fail "release builds need glibc 2.35 or newer, and this system has $glibc. Build from source: $from_source"
                ;;
        esac
        target="$arch-unknown-linux-gnu"
        ;;
    Darwin)
        # A shell under Rosetta reports x86_64 on Apple silicon.
        if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
            arch=aarch64
        fi
        target="$arch-apple-darwin"
        ;;
    *) fail "no release build for $os. Build from source: $from_source" ;;
esac

# Release archives have no version in their names, so the latest release's
# archive has a fixed URL.
if [ "$version" = latest ]; then
    download="$releases/latest/download"
    release="the latest release"
else
    download="$releases/download/v${version#v}"
    release="release v${version#v}"
fi

name="yapi-$target.tar.gz"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
echo "Downloading $name from $release"
curl -fsSL -o "$work/$name" "$download/$name" ||
    fail "found no $name in $release at $releases. Build from source: $from_source"
curl -fsSL -o "$work/$name.sha256" "$download/$name.sha256" ||
    fail "could not download the checksum for $name"
expected=$(cut -d ' ' -f 1 <"$work/$name.sha256")
actual=$(sha256 "$work/$name")
[ "$expected" = "$actual" ] || fail "$name does not match its SHA-256 checksum"

mkdir "$work/unpacked"
tar -xzf "$work/$name" -C "$work/unpacked"
[ -f "$work/unpacked/yapi" ] || fail "$name does not contain yapi"
installed=$("$work/unpacked/yapi" --version) || fail "the downloaded yapi does not run on this system"
mkdir -p "$dir"
# Copy, then rename, so a running yapi is replaced rather than overwritten.
cp "$work/unpacked/yapi" "$dir/.yapi.$$"
chmod 755 "$dir/.yapi.$$"
mv -f "$dir/.yapi.$$" "$dir/yapi"
echo "Installed yapi $installed to $dir/yapi"

case ":${PATH:-}:" in
    *":$dir:"*) ;;
    *) echo "Add $dir to PATH to run yapi, for example in your shell's profile: export PATH=\"$dir:\$PATH\"" ;;
esac
