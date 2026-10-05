#!/usr/bin/env python3
"""Send one prompt to yapi in RPC mode and print the final answer.

Usage: ask.py [--yapi PATH] [--model PROVIDER/ID] [--keep-session] PROMPT [-- YAPI_ARGS...]

Arguments after `--` go to yapi unchanged, for example `--tools read,grep`.
Exits with 1 and the error on stderr when yapi rejects the prompt or the run
ends with an error.
"""

import argparse
import json
import subprocess
import sys


def main() -> int:
    argv = sys.argv[1:]
    extra = []
    if "--" in argv:
        split = argv.index("--")
        argv, extra = argv[:split], argv[split + 1 :]
    parser = argparse.ArgumentParser(description="Send one prompt to yapi and print the answer.")
    parser.add_argument("--yapi", default="yapi", help="the yapi executable")
    parser.add_argument("--model", help="the model, as provider/id[:thinking]")
    parser.add_argument("--keep-session", action="store_true", help="save the session")
    parser.add_argument("prompt")
    args = parser.parse_args(argv)

    command = [args.yapi, "--mode", "rpc"]
    if not args.keep_session:
        command.append("--no-session")
    if args.model:
        command += ["--model", args.model]
    proc = subprocess.Popen(
        command + extra,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )

    def send(message: dict) -> None:
        proc.stdin.write(json.dumps(message) + "\n")
        proc.stdin.flush()

    def fail(message: str) -> int:
        proc.stdin.close()
        stderr = proc.stderr.read()
        proc.wait()
        print(message or stderr.strip() or "yapi stopped", file=sys.stderr)
        return 1

    # yapi exits when stdin closes, so it stays open until the run ends.
    send({"id": "prompt", "type": "prompt", "message": args.prompt})
    error = None
    for line in proc.stdout:
        event = json.loads(line)
        kind = event.get("type")
        if kind == "response" and event.get("id") == "prompt" and not event.get("success"):
            return fail(event.get("error", "prompt rejected"))
        if kind == "message_end":
            message = event.get("message", {})
            if message.get("role") == "assistant" and message.get("stopReason") == "error":
                error = message.get("errorMessage", "the run failed")
        if kind == "agent_end":
            break
    else:
        return fail("")
    if error:
        return fail(error)

    send({"id": "answer", "type": "get_last_assistant_text"})
    for line in proc.stdout:
        event = json.loads(line)
        if event.get("type") == "response" and event.get("id") == "answer":
            print(event.get("data", {}).get("text", ""))
            break
    proc.stdin.close()
    proc.wait()
    return 0


if __name__ == "__main__":
    sys.exit(main())
