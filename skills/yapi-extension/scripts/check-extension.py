#!/usr/bin/env python3
"""Load extensions in yapi without a model or the network, and report what
they registered and every load error.

Usage: check-extension.py [--yapi PATH] EXTENSION [EXTENSION...]

Each EXTENSION is a `.ts`, `.js` or `.wasm` file or a package folder. yapi
starts in RPC mode with only these extensions, in a fresh agent directory, so
the user's settings and packages do not interfere. Exits with 1 when an
extension fails to load.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile


def main() -> int:
    parser = argparse.ArgumentParser(description="Check that yapi extensions load.")
    parser.add_argument("--yapi", default="yapi", help="the yapi executable")
    parser.add_argument("extensions", nargs="+")
    args = parser.parse_args()

    command = [args.yapi, "--mode", "rpc", "--no-session", "--offline", "-ne"]
    for extension in args.extensions:
        command += ["-e", os.path.abspath(extension)]
    # stderr goes to a file: a pipe nobody reads fills up with extension logs
    # and stops yapi.
    with tempfile.TemporaryDirectory(prefix="yapi-check-") as agent, tempfile.TemporaryFile() as log:
        env = dict(os.environ, YAPI_CODING_AGENT_DIR=agent, PI_OFFLINE="1")
        proc = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=log,
            text=True,
            env=env,
        )
        proc.stdin.write(json.dumps({"id": "commands", "type": "get_commands"}) + "\n")
        proc.stdin.flush()
        errors = []
        commands = None
        for line in proc.stdout:
            event = json.loads(line)
            if event.get("type") == "extension_error":
                errors.append(f"{event.get('extensionPath')}: {event.get('error')}")
            if event.get("type") == "response" and event.get("id") == "commands":
                commands = event.get("data", {}).get("commands", [])
                break
        proc.stdin.close()
        code = proc.wait()
        log.seek(0)
        stderr = log.read().decode(errors="replace")

    if commands is None or code != 0:
        print(stderr.strip() or f"yapi exited with {code}", file=sys.stderr)
        return 1
    # `-ne` keeps the built-in extensions out, so every extension command is theirs.
    loaded = [c for c in commands if c.get("source") == "extension"]
    if errors:
        print(f"Loaded with {len(errors)} error(s).")
    else:
        print(f"Loaded {len(args.extensions)} extension(s).")
    for command in loaded:
        print(f"  /{command['name']}: {command.get('description') or ''}")
    if stderr.strip():
        print(stderr.strip(), file=sys.stderr)
    for error in errors:
        print(f"Error: {error}", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
