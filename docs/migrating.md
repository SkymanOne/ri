# Coming from pi

ri reads pi's formats but keeps its own folders, so the two tools never write the same files. Copy your pi setup once:

```sh
ri import pi
```

The command copies settings, credentials, models, key bindings, MCP servers, trust decisions, sessions, prompt templates, skills, themes, extensions and packages from `~/.pi/agent`, and the current project's `.pi` folder, into ri's folders. Files that ri already has are kept.

After that, ri and pi work side by side. Session files written by either one open in the other.

## Names that change

| pi | ri |
|---|---|
| `~/.pi/agent` | `~/.ri/agent` |
| Project `.pi` folder | Project `.ri` folder |
| `PI_CODING_AGENT_DIR` | `RI_CODING_AGENT_DIR` |
| `PI_CODING_AGENT_SESSION_DIR` | `RI_CODING_AGENT_SESSION_DIR` |
| `pi install`, `pi config`, `pi mcp`, `pi auth` | `ri install`, `ri config`, `ri mcp`, `ri auth` |

Provider keys and `PI_OFFLINE` keep their names. Command-line flags, slash commands and the JSON and RPC protocols are the same as pi's.

## What differs

A few behaviors differ on purpose, such as the startup header and features tied to pi's online services. [Differences from pi](compat.md) lists each one with its reason.
