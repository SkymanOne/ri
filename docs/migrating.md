# Coming from Pi

yapi reads Pi's formats but keeps its own folders, so the two tools never write the same files. Copy your Pi setup once:

```sh
yapi import pi
```

The command copies settings, credentials, models, key bindings, MCP servers, trust decisions, the global `AGENTS.md` or `CLAUDE.md`, `SYSTEM.md` and `APPEND_SYSTEM.md`, sessions, prompt templates, skills, themes, extensions and packages from Pi's agent folder into yapi's. That folder is `~/.pi/agent`, or `PI_CODING_AGENT_DIR` when it is set. The command also copies the current project's `.pi` folder into `.yapi`. Files that yapi already has are kept.

After that, yapi and Pi work side by side. Session files written by either one open in the other.

## Names that change

| Pi | yapi |
|---|---|
| `~/.pi/agent` | `~/.yapi/agent` |
| Project `.pi` folder | Project `.yapi` folder |
| `PI_CODING_AGENT_DIR` | `YAPI_CODING_AGENT_DIR` |
| `PI_CODING_AGENT_SESSION_DIR` | `YAPI_CODING_AGENT_SESSION_DIR` |
| `pi install`, `pi config`, `pi mcp`, `pi auth` | `yapi install`, `yapi config`, `yapi mcp`, `yapi auth` |

Only the two directory variables are renamed. Provider keys and every other `PI_*` variable that yapi reads, such as `PI_OFFLINE` and `PI_CACHE_RETENTION`, keep their names. Command-line flags, slash commands and the JSON and RPC protocols are the same as Pi's.

## What differs

A few behaviors differ on purpose, such as the startup header and features tied to Pi's online services. [Differences from Pi](compat.md) lists each one with its reason.
