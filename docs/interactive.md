# Interactive mode

Running `yapi` with no message starts the interactive terminal interface. The transcript fills the screen, and the editor at the bottom takes your message.

## Keys

These are the defaults. `/hotkeys` shows the bindings in effect, and `~/.yapi/agent/keybindings.json` changes them in Pi's format.

| Key | Action |
|---|---|
| Enter | Send the message. While yapi works, it steers the current turn. |
| Alt+Enter | Queue a follow-up for when yapi finishes |
| Shift+Enter | Insert a new line |
| Escape | Interrupt the agent. Twice on an empty editor opens the session tree. |
| Ctrl+C | Clear the editor. Twice quits. |
| Ctrl+D | Quit when the editor is empty |
| Ctrl+O | Expand or collapse tool output |
| Ctrl+T | Show or hide thinking |
| Ctrl+G | Write the message in `$EDITOR` |
| Ctrl+L | Open the model selector |
| Ctrl+P | Cycle models |
| Shift+Tab | Cycle the thinking level |
| Tab | Complete paths and commands |
| Up | Recall earlier messages |

## Slash commands

Type `/` to list commands. The most used ones:

| Command | Action |
|---|---|
| `/model`, `/thinking` | Switch the model or thinking level |
| `/settings` | Change settings such as the theme, auto-compaction or fullscreen mode |
| `/scoped-models` | Choose which models Ctrl+P cycles through |
| `/new`, `/resume` | Start a session or open an earlier one |
| `/tree` | Browse the session's branches and jump to an earlier point |
| `/fork`, `/clone` | Branch from an earlier message or duplicate the session |
| `/compact` | Summarize older context to free up the context window |
| `/name`, `/session` | Name the session or show its file, tokens and cost |
| `/copy`, `/export` | Copy the last answer or export the session to HTML |
| `/login`, `/logout` | Manage provider credentials |
| `/import` | Copy a session file into the session folder and resume it |
| `/llama` | Load, unload and download models on a llama.cpp server |
| `/mcp` | Show MCP server status |
| `/trust` | Change whether the project's `.yapi` folder is trusted |
| `/reload` | Reload settings, keybindings, extensions, skills and themes |
| `/hotkeys` | Show every key binding |
| `/debug` | Write the rendered screen and the session's messages to `~/.yapi/agent/yapi-debug.log` |
| `/quit` | Quit yapi |

Prompt templates, skills (`/skill:name`) and extension commands appear in the same list.

## Message shortcuts

- `@path` attaches a file. Tab completes the path.
- `!command` runs a shell command and adds its output to the conversation.
- `!!command` runs a shell command without adding its output.

## Display modes

yapi draws in fullscreen mode by default. Regular mode keeps the transcript in the terminal's scrollback instead. Switch with `/settings` or `--tui-mode regular`.
