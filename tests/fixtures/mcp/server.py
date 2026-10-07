#!/usr/bin/env python3
"""MCP test server for yapi's tests: the same tools, resources and replies over
stdio (default) or streamable HTTP (`--http`, which prints its URL on the first
line of stdout). Python standard library only.

With `--http`, `--token <token>` accepts only that bearer token, and `--oauth`
also serves an OAuth authorization server that accepts the tokens it issues:
protected resource and authorization server metadata, dynamic client
registration, an authorization endpoint that approves at once by redirecting
with a code, and a token endpoint for codes (PKCE S256) and rotating refresh
tokens. `POST /test/expire` revokes the access tokens issued so far and
`GET /test/stats` counts registrations and refreshes.

Tools:
  echo      text content
  add       structured content only
  fail      an isError result
  progress  progress notifications, then text (HTTP: as an SSE response)
  picture   an image block, a resource link and an embedded text resource
  big       text over pi's 20KB model limit
  env       the value of an environment variable
  sleep     waits the given seconds, then answers
  exit      ends the server without answering
"""

import base64
import hashlib
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlsplit

PROTOCOL_VERSIONS = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"]
PIXEL = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="

TOOLS = [
    {
        "name": "echo",
        "description": "Echo the given text back.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string", "description": "Text to echo"}},
            "required": ["text"],
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "add",
        "title": "Add numbers",
        "description": "Add two numbers and return the sum as structured content.",
        "inputSchema": {
            "type": "object",
            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"],
        },
        "outputSchema": {"type": "object", "properties": {"sum": {"type": "number"}}},
    },
    {"name": "fail", "description": "Always fails.", "inputSchema": {"type": "object"}},
    {
        "name": "progress",
        "description": "Report progress twice, then finish.",
        "inputSchema": {"properties": {}},
    },
    {"name": "picture", "description": "Return an image and resources.", "inputSchema": {"type": "object"}},
    {"name": "big", "description": "Return a long text.", "inputSchema": {"type": "object"}},
    {
        "name": "sleep",
        "description": "Wait, then answer.",
        "inputSchema": {"type": "object", "properties": {"seconds": {"type": "number"}}},
    },
    {"name": "exit", "description": "End the server.", "inputSchema": {"type": "object"}},
    {
        "name": "env",
        "description": "Read an environment variable of the server.",
        "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}},
    },
]

RESOURCES = [
    {"uri": "test://notes/readme", "name": "readme", "mimeType": "text/plain", "description": "The notes"},
]


def text(value):
    return {"content": [{"type": "text", "text": value}]}


def call_tool(name, args, progress):
    if name == "echo":
        return text(args.get("text", ""))
    if name == "add":
        return {"structuredContent": {"sum": args.get("a", 0) + args.get("b", 0)}}
    if name == "fail":
        return {"content": [{"type": "text", "text": "Something went wrong"}], "isError": True}
    if name == "progress":
        progress(1, 2, "Halfway")
        progress(2, 2, None)
        return text("Done")
    if name == "picture":
        return {
            "content": [
                {"type": "image", "data": PIXEL, "mimeType": "image/png"},
                {"type": "resource_link", "uri": "test://notes/readme", "name": "readme", "mimeType": "text/plain"},
                {"type": "resource", "resource": {"uri": "test://notes/inline", "text": "Inline note"}},
            ]
        }
    if name == "big":
        return text("".join(f"line {index:05d} of the long output\n" for index in range(2000)))
    if name == "sleep":
        time.sleep(args.get("seconds", 0))
        return text("Slept")
    if name == "exit":
        os._exit(0)
    if name == "env":
        return text(os.environ.get(args.get("name", ""), "<unset>"))
    raise KeyError(name)


def handle(message, notify):
    """The response to `message`, or None for notifications."""
    method = message.get("method")
    if "id" not in message or method is None:
        return None
    request_id = message["id"]
    params = message.get("params") or {}

    def result(value):
        return {"jsonrpc": "2.0", "id": request_id, "result": value}

    def error(code, text_):
        return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": text_}}

    if method == "initialize":
        requested = params.get("protocolVersion")
        version = requested if requested in PROTOCOL_VERSIONS else PROTOCOL_VERSIONS[0]
        return result(
            {
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": True}, "resources": {}},
                "serverInfo": {"name": "yapi-test-server", "version": "1.0.0"},
                "instructions": "Test tools for yapi.\nThey echo, add and fail.",
            }
        )
    if method == "ping":
        return result({})
    if method == "tools/list":
        return result({"tools": TOOLS})
    if method == "resources/list":
        return result({"resources": RESOURCES})
    if method == "resources/templates/list":
        return error(-32601, "Method not found")
    if method == "resources/read":
        uri = params.get("uri")
        if uri == "test://notes/readme":
            return result({"contents": [{"uri": uri, "mimeType": "text/plain", "text": "Read me first."}]})
        return error(-32602, f"Unknown resource: {uri}")
    if method == "tools/call":
        token = (params.get("_meta") or {}).get("progressToken")

        def progress(done, total, note):
            if token is None:
                return
            update = {"progressToken": token, "progress": done, "total": total}
            if note:
                update["message"] = note
            notify({"jsonrpc": "2.0", "method": "notifications/progress", "params": update})

        try:
            return result(call_tool(params.get("name"), params.get("arguments") or {}, progress))
        except KeyError:
            return error(-32602, f"Unknown tool: {params.get('name')}")
    return error(-32601, f"Method not found: {method}")


def serve_stdio():
    lock = threading.Lock()

    def write(message):
        with lock:
            sys.stdout.write(json.dumps(message) + "\n")
            sys.stdout.flush()

    for line in sys.stdin:
        if not line.strip():
            continue
        response = handle(json.loads(line), write)
        if response is not None:
            write(response)


class OAuth:
    """The authorization server's state."""

    def __init__(self):
        self.lock = threading.Lock()
        self.clients = {}
        self.codes = {}
        self.access = set()
        self.refresh = set()
        self.issued = 0
        self.registrations = 0
        self.refreshes = 0

    def tokens(self, scope):
        self.issued += 1
        access, refresh = f"access-{self.issued}", f"refresh-{self.issued}"
        self.access.add(access)
        self.refresh.add(refresh)
        return {"access_token": access, "token_type": "Bearer", "expires_in": 3600, "refresh_token": refresh, "scope": scope}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    token = None
    oauth = None

    def log_message(self, *args):
        pass

    def reply(self, status, body=b"", content_type=None, headers=()):
        self.send_response(status)
        if content_type:
            self.send_header("Content-Type", content_type)
        for name, value in headers:
            self.send_header(name, value)
        self.send_header("Mcp-Session-Id", "test-session")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def json_reply(self, status, value):
        self.reply(status, json.dumps(value).encode(), "application/json")

    def base(self):
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def body(self):
        return self.rfile.read(int(self.headers.get("Content-Length", "0")))

    def authorized(self):
        """Whether the request may reach /mcp; answers 401 when it may not."""
        bearer = self.headers.get("Authorization", "")
        if self.token is not None:
            if bearer == f"Bearer {self.token}":
                return True
            self.reply(401, b"Unauthorized", "text/plain", [("WWW-Authenticate", "Bearer")])
            return False
        if self.oauth is None:
            return True
        with self.oauth.lock:
            if bearer.startswith("Bearer ") and bearer[7:] in self.oauth.access:
                return True
        metadata = f"{self.base()}/.well-known/oauth-protected-resource/mcp"
        self.reply(401, b"Unauthorized", "text/plain", [("WWW-Authenticate", f'Bearer resource_metadata="{metadata}"')])
        return False

    def oauth_get(self, url):
        base, query = self.base(), parse_qs(url.query)
        if url.path == "/.well-known/oauth-protected-resource/mcp":
            return self.json_reply(
                200, {"resource": f"{base}/mcp", "authorization_servers": [base], "scopes_supported": ["mcp:read"]}
            )
        if url.path == "/.well-known/oauth-authorization-server":
            return self.json_reply(
                200,
                {
                    "issuer": base,
                    "authorization_endpoint": f"{base}/authorize",
                    "token_endpoint": f"{base}/token",
                    "registration_endpoint": f"{base}/register",
                    "response_types_supported": ["code"],
                    "grant_types_supported": ["authorization_code", "refresh_token"],
                    "token_endpoint_auth_methods_supported": ["none"],
                    "code_challenge_methods_supported": ["S256"],
                },
            )
        if url.path == "/authorize":
            first = lambda name: query.get(name, [""])[0]
            with self.oauth.lock:
                client = self.oauth.clients.get(first("client_id"))
                if client is None or first("redirect_uri") not in client["redirect_uris"]:
                    return self.json_reply(400, {"error": "invalid_request"})
                code = f"code-{len(self.oauth.codes) + 1}"
                self.oauth.codes[code] = (first("code_challenge"), first("redirect_uri"), first("scope"))
            location = first("redirect_uri") + "?" + urlencode({"code": code, "state": first("state")})
            return self.reply(302, headers=[("Location", location)])
        if url.path == "/test/stats":
            return self.json_reply(200, {"registrations": self.oauth.registrations, "refreshes": self.oauth.refreshes})
        return self.reply(404)

    def oauth_post(self, url):
        if url.path == "/register":
            metadata = json.loads(self.body())
            with self.oauth.lock:
                self.oauth.registrations += 1
                client_id = f"client-{self.oauth.registrations}"
                self.oauth.clients[client_id] = metadata
            return self.json_reply(201, {**metadata, "client_id": client_id})
        if url.path == "/token":
            form = {key: values[0] for key, values in parse_qs(self.body().decode()).items()}
            with self.oauth.lock:
                if form.get("grant_type") == "refresh_token":
                    if form.get("refresh_token") not in self.oauth.refresh:
                        return self.json_reply(400, {"error": "invalid_grant"})
                    self.oauth.refresh.discard(form["refresh_token"])
                    self.oauth.refreshes += 1
                    return self.json_reply(200, self.oauth.tokens("mcp:read"))
                challenge, redirect, scope = self.oauth.codes.pop(form.get("code"), (None, None, None))
                digest = hashlib.sha256(form.get("code_verifier", "").encode()).digest()
                verified = base64.urlsafe_b64encode(digest).rstrip(b"=").decode()
                if challenge is None or verified != challenge or form.get("redirect_uri") != redirect:
                    return self.json_reply(400, {"error": "invalid_grant"})
                return self.json_reply(200, self.oauth.tokens(scope))
        if url.path == "/test/expire":
            with self.oauth.lock:
                self.oauth.access.clear()
            return self.reply(204)
        return self.reply(404)

    def do_GET(self):
        url = urlsplit(self.path)
        if self.oauth is not None and url.path != "/mcp":
            return self.oauth_get(url)
        if self.authorized():
            self.reply(405)

    def do_DELETE(self):
        self.reply(200)

    def do_POST(self):
        url = urlsplit(self.path)
        if self.oauth is not None and url.path != "/mcp":
            return self.oauth_post(url)
        if not self.authorized():
            self.body()
            return
        message = json.loads(self.body())
        notes = []
        response = handle(message, notes.append)
        if response is None:
            self.reply(202)
        elif notes:
            events = "".join(f"event: message\ndata: {json.dumps(item)}\n\n" for item in [*notes, response])
            self.reply(200, events.encode(), "text/event-stream")
        else:
            self.reply(200, json.dumps(response).encode(), "application/json")


def serve_http():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print(f"http://127.0.0.1:{server.server_address[1]}/mcp", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    if "--token" in sys.argv:
        Handler.token = sys.argv[sys.argv.index("--token") + 1]
    if "--oauth" in sys.argv:
        Handler.oauth = OAuth()
    if "--http" in sys.argv:
        serve_http()
    else:
        serve_stdio()
