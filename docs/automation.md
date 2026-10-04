# Print, JSON and RPC modes

ri runs without its interface for scripts, editors and other programs. The output formats and protocols are pi's, so pi's clients work with ri unchanged.

## Print mode

`-p` sends one message, prints the answer and exits:

```sh
ri -p "Summarize the open TODOs in src/"
git diff | ri -p "Write a commit message for this diff"
ri -p @report.md @chart.png "What does the chart show?"
```

Piped input becomes part of the first message, and `@path` attaches files and images.

## JSON mode

`--mode json` prints one JSON event per line for the whole run: message updates, tool calls, tool results and the final state. The events are described in pi's [JSON documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/json.md).

```sh
ri --mode json -p "List the crates in this workspace"
```

## RPC mode

`--mode rpc` keeps ri running and reads commands from stdin, one JSON object per line. Responses and events go to stdout. Clients can prompt, steer, abort, switch models and sessions, and answer extension dialogs. The commands are listed in pi's [RPC documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/rpc.md).

```sh
ri --mode rpc --no-session
```

pi's `RpcClient` example drives ri in the test suite.

## Useful flags

| Flag | Effect |
|---|---|
| `--no-session` | Do not save the run |
| `--model <pattern>` | Choose the model |
| `--tools read,grep,find,ls` | Allow only these tools, here read-only ones |
| `--no-tools` | Disable all tools |
| `-ne` | Start without extensions |
| `--offline` | Skip startup network work |
