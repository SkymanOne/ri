# Coming from Pi

yapi reads Pi's formats but keeps its own folders, so the two tools never write the same files. Copy your Pi setup once:

```sh
yapi import pi
```

The command copies settings, credentials, models, key bindings, MCP servers, trust decisions, sessions, prompt templates, skills, themes, extensions and packages from `~/.pi/agent`, and the current project's `.pi` folder, into yapi's folders. Files that yapi already has are kept.

After that, yapi and Pi work side by side. Session files written by either one open in the other.

## Names that change

| Pi | yapi |
|---|---|
| `~/.pi/agent` | `~/.yapi/agent` |
| Project `.pi` folder | Project `.yapi` folder |
| `PI_CODING_AGENT_DIR` | `YAPI_CODING_AGENT_DIR` |
| `PI_CODING_AGENT_SESSION_DIR` | `YAPI_CODING_AGENT_SESSION_DIR` |
| `pi install`, `pi config`, `pi mcp`, `pi auth` | `yapi install`, `yapi config`, `yapi mcp`, `yapi auth` |

Provider keys and `PI_OFFLINE` keep their names. Command-line flags, slash commands and the JSON and RPC protocols are the same as Pi's.

## What differs

A few behaviors differ on purpose, such as the startup header and features tied to Pi's online services. [Differences from Pi](compat.md) lists each one with its reason.
