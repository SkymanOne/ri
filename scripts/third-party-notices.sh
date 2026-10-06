#!/bin/sh
# Writes the notices that release archives ship next to the yapi binary: the
# licenses of the Rust crates in the binary and in the JavaScript runtime it
# embeds, found by cargo-about with about.toml and about.hbs, and the notices
# of the vendored JavaScript packages and of pi's HTML export template, which
# the binary embeds.
#
# Usage: scripts/third-party-notices.sh <out-file>
# Needs cargo-about.
set -eu

if [ $# -ne 1 ]; then
    echo "Usage: $0 <out-file>" >&2
    exit 1
fi
root=$(cd "$(dirname "$0")/.." && pwd)
about() {
    cargo about generate --locked --fail -c "$root/about.toml" -m "$@" "$root/about.hbs"
}

{
    echo "yapi includes the third-party software listed below, under the licenses shown."
    echo
    echo "================================================================================"
    echo "Rust crates in the yapi binary"
    echo "================================================================================"
    about "$root/crates/yapi/Cargo.toml"
    echo
    echo "================================================================================"
    echo "Rust crates in the embedded JavaScript runtime (guest/yapi-js)"
    echo "================================================================================"
    about "$root/guest/yapi-js/Cargo.toml" --target wasm32-wasip2
    echo
    echo "================================================================================"
    echo "JavaScript packages in the embedded JavaScript runtime"
    echo "================================================================================"
    echo
    cat "$root/guest/yapi-js/js/vendor/LICENSES.md"
    echo
    echo "================================================================================"
    echo "Pi's HTML export template in the yapi binary"
    echo "================================================================================"
    echo
    cat "$root/crates/yapi/assets/export-html/LICENSES.md"
} >"$1"
