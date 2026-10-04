#!/usr/bin/env python3
"""MCP test server for ri's tests: the same tools, resources and replies over
stdio (default) or streamable HTTP (`--http`, which prints its URL on the first
line of stdout). Python standard library only.

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

import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

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
                "serverInfo": {"name": "ri-test-server", "version": "1.0.0"},
                "instructions": "Test tools for ri.\nThey echo, add and fail.",
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


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, status, body=b"", content_type=None):
        self.send_response(status)
        if content_type:
            self.send_header("Content-Type", content_type)
        self.send_header("Mcp-Session-Id", "test-session")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.reply(405)

    def do_DELETE(self):
        self.reply(200)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        message = json.loads(self.rfile.read(length))
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
    if "--http" in sys.argv:
        serve_http()
    else:
        serve_stdio()
