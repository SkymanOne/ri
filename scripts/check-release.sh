#!/bin/sh
# Installs this system's release archive from <dist-dir> as users would: with
# install.sh, as the latest release and as release <tag>, and with
# cargo-binstall when it is on PATH. The archives are served over HTTPS from
# 127.0.0.1 in the layout of GitHub's release downloads, with a throwaway
# certificate authority.
#
# Usage: scripts/check-release.sh <dist-dir> <tag>
# Needs curl, openssl and python3.
set -eu

if [ $# -ne 2 ]; then
    echo "Usage: $0 <dist-dir> <tag>" >&2
    exit 1
fi
dist=$(cd "$1" && pwd)
tag=$2
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
server=
cleanup() {
    [ -z "$server" ] || kill "$server" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

# GitHub serves the latest release's files under latest/download as well.
for path in "download/$tag" latest/download; do
    mkdir -p "$work/site/SkymanOne/ri/releases/$path"
    cp "$dist"/yapi-* "$work/site/SkymanOne/ri/releases/$path/"
done

# A certificate authority and a certificate for 127.0.0.1 that it signs.
cd "$work"
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 1 \
    -subj "/CN=yapi release check" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -keyout key.pem -out server.csr \
    -subj "/CN=127.0.0.1" 2>/dev/null
printf 'subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\n' >server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out cert.pem -days 1 -extfile server.ext 2>/dev/null

cat >serve.py <<'EOF'
import functools, http.server, socketserver, ssl


class Server(http.server.ThreadingHTTPServer):
    # HTTPServer.server_bind looks up the address's host name, which can take
    # many seconds on macOS. Nothing here uses the name.
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]


handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory="site")
httpd = Server(("127.0.0.1", 0), handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain("cert.pem", "key.pem")
httpd.socket = context.wrap_socket(httpd.socket, server_side=True)
print(httpd.server_address[1], flush=True)
httpd.serve_forever()
EOF
python3 serve.py >port 2>server.log &
server=$!
waited=0
while [ ! -s port ] && [ "$waited" -lt 60 ]; do
    sleep 1
    waited=$((waited + 1))
done
if [ ! -s port ]; then
    echo "The HTTPS server did not start within 60 s. $(python3 --version 2>&1), log:" >&2
    cat server.log >&2
    exit 1
fi
repo="https://127.0.0.1:$(cat port)/SkymanOne/ri"
version=${tag#v}

check() {
    found=$("$1" --version)
    if [ "$found" != "$version" ]; then
        echo "$1 --version printed '$found', not '$version'" >&2
        exit 1
    fi
    echo "ok: $2 installed yapi $found"
}

CURL_CA_BUNDLE="$work/ca.pem" YAPI_RELEASES_URL="$repo/releases" \
    sh "$root/install.sh" --to "$work/latest/bin"
check "$work/latest/bin/yapi" "install.sh, latest release,"
CURL_CA_BUNDLE="$work/ca.pem" YAPI_RELEASES_URL="$repo/releases" \
    sh "$root/install.sh" --version "$tag" --to "$work/tagged/bin"
check "$work/tagged/bin/yapi" "install.sh, release $tag,"

if command -v cargo-binstall >/dev/null 2>&1; then
    # The repository's manifests, pointing at this server instead of GitHub.
    manifest="$work/manifest"
    mkdir -p "$manifest/crates/yapi/src"
    sed "s|^repository = .*|repository = \"$repo\"|" "$root/Cargo.toml" >"$manifest/Cargo.toml"
    cp "$root/crates/yapi/Cargo.toml" "$manifest/crates/yapi/"
    : >"$manifest/crates/yapi/src/main.rs"
    cargo-binstall --manifest-path "$manifest" yapi --no-confirm --disable-telemetry \
        --root "$work/binstall" --root-certificates "$work/ca.pem"
    check "$work/binstall/bin/yapi" cargo-binstall
else
    echo "skipped: cargo-binstall is not on PATH"
fi
