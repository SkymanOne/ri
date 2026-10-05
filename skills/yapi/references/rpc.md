# yapi RPC mode

`yapi --mode rpc` reads one JSON command per line on stdin and writes JSON lines on stdout: a response for each command, with the command's `id`, and the same events as JSON mode. The protocol is Pi's, described in full in Pi's [RPC documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/rpc.md).

yapi exits when stdin closes. Keep stdin open until you have read what you need, for a prompt usually the `agent_end` event.

## A minimal client

```python
import json
import subprocess

proc = subprocess.Popen(
    ["yapi", "--mode", "rpc", "--no-session"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    text=True,
)


def send(command):
    proc.stdin.write(json.dumps(command) + "\n")
    proc.stdin.flush()


send({"id": "1", "type": "prompt", "message": "Run the tests and summarize failures"})
for line in proc.stdout:
    event = json.loads(line)
    if event.get("type") == "response" and not event.get("success", True):
        raise RuntimeError(event.get("error"))
    if event.get("type") == "agent_end":
        break

send({"id": "2", "type": "get_last_assistant_text"})
for line in proc.stdout:
    event = json.loads(line)
    if event.get("type") == "response" and event.get("id") == "2":
        print(event["data"].get("text", ""))
        break

proc.stdin.close()
proc.wait()
```

## Commands

Every command is an object with `type` and an optional `id`, which the response repeats. Field names are camelCase.

| `type` | Fields | Does |
|---|---|---|
| `prompt` | `message`, optional `images`, optional `streamingBehavior` (`steer` or `followUp`) while a run is active | Sends a user message |
| `steer` | `message`, optional `images` | Interrupts the current run after its tool calls |
| `follow_up` | `message`, optional `images` | Queues a message for when the run ends |
| `abort` | | Stops the current run |
| `clear_queue` | | Drops queued messages |
| `get_state` | | Model, thinking level, session and queue state |
| `get_messages`, `get_last_assistant_text` | | The conversation, or the last answer's text |
| `set_model` | `provider`, `modelId` | Switches the model |
| `cycle_model`, `get_available_models` | | |
| `set_thinking_level` | `level` (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`) | |
| `cycle_thinking_level`, `get_available_thinking_levels` | | |
| `set_steering_mode`, `set_follow_up_mode` | `mode` (`all` or `one-at-a-time`) | |
| `compact` | optional `customInstructions` | Summarizes old context |
| `set_auto_compaction`, `set_auto_retry` | `enabled` | |
| `abort_retry` | | |
| `bash` | `command`, optional `excludeFromContext` | Runs a shell command as the `!` prefix does |
| `abort_bash` | | |
| `new_session` | optional `parentSession` | |
| `switch_session` | `sessionPath` | |
| `fork` | `entryId` | |
| `clone`, `get_fork_messages`, `get_tree` | | |
| `get_entries` | optional `since` | Session entries |
| `set_session_name` | `name` | |
| `get_session_stats` | | Token use and cost |
| `export_html` | optional `outputPath` | |
| `get_commands` | | Slash commands from extensions, prompt templates and skills |

Invalid arguments get an error response that names the problem, where Pi would coerce them.

## Extension dialogs

Extensions that open dialogs send `extension_ui_request` lines. Answer each with an `extension_ui_response` carrying its `id`, as Pi's [extension UI protocol](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/rpc-extension-ui.md) describes, or start yapi with `-ne` when no extensions are needed.
