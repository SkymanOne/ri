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
| Ctrl+V | Paste copied files' paths (macOS), an image or text from the clipboard. An image is saved to a temporary file, and its path goes in the editor. |
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
| `/mcp` | Manage MCP servers: sign in, reconnect, enable or disable them, and change their exposure |
| `/trust` | Change whether the project's `.yapi` folder is trusted |
| `/reload` | Reload settings, keybindings, extensions, skills and themes |
| `/hotkeys` | Show every key binding |
| `/debug` | Write the rendered screen, the session's messages and the last 200 lines of extension output to `~/.yapi/agent/yapi-debug.log`. Read the file before you attach it to a bug report, because messages and extension output can hold private data. |
| `/quit` | Quit yapi |

Prompt templates, skills (`/skill:name`) and extension commands appear in the same list.

## Message shortcuts

- `@path` attaches a file. Tab completes the path.
- `!command` runs a shell command and adds its output to the conversation.
- `!!command` runs a shell command without adding its output.

## Display modes

yapi draws in fullscreen mode by default. Regular mode keeps the transcript in the terminal's scrollback instead. Switch with `/settings` or `--tui-mode regular`.

In fullscreen mode yapi handles the mouse itself. The wheel scrolls the transcript, five times as far with Alt held. A fast spin scrolls up to six lines per step, except in a local macOS terminal, which speeds up the wheel itself. Set "Fullscreen wheel scrolling" in `/settings` to a fixed number of lines per step instead. Drag to select text. A double click selects a word, keeping a path or a hyphenated name whole, and a triple click selects a line. Dragging past the top or bottom of the transcript scrolls it. Releasing the button copies the selection and shows "Copied!" at the top right. With "Fullscreen copy on select" off in `/settings`, the selection stays highlighted until Ctrl+X copies it.

When the terminal shows [hyperlinks](configuration.md#links), a click on a web or mail link opens it in the browser or mail app, and a click on a file link shows the file in its folder without opening it. Other links do not open, so a click never starts a program. A click in the editor moves its cursor there. In the completion list, a click chooses an item and the wheel moves the highlight. The lists of `/settings`, `/thinking` and their submenus work the same way. A click in a selector's search field or a dialog's input moves its cursor. A click on a tool call, a run of thinking, a skill or a compaction or branch summary expands or collapses that one item. Ctrl+O then sets every item to its new state, and Ctrl+T shows or hides all thinking again. Click "Jump to latest message" to scroll to the end. Drag the scrollbar's thumb, or click its track to jump there. Pointing at the scrollbar shows it when "Fullscreen scrollbar" is `auto`. Inside tmux, Zellij or GNU Screen the terminal reports the pointer only while a button is held, so pointing does not show it there.
