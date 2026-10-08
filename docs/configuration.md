# Configuration

yapi keeps global state in `~/.yapi/agent` and project state in a `.yapi` folder. Set `YAPI_CODING_AGENT_DIR` to move the global folder. Every file uses Pi's format, described in Pi's [settings documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/settings.md).

| What | Where |
|---|---|
| Settings | `~/.yapi/agent/settings.json`, overridden by `.yapi/settings.json` |
| Project instructions | `AGENTS.md` or `CLAUDE.md` in the project, its parent folders and `~/.yapi/agent` |
| System prompt | `SYSTEM.md` replaces it and `APPEND_SYSTEM.md` extends it, in `.yapi/` or `~/.yapi/agent/` |
| Skills | `~/.yapi/agent/skills`, `~/.agents/skills`, `.yapi/skills` and `.agents/skills` |
| Prompt templates | `~/.yapi/agent/prompts` and `.yapi/prompts` |
| Themes | `~/.yapi/agent/themes` and `.yapi/themes` |
| Key bindings | `~/.yapi/agent/keybindings.json` |
| Models and providers | `~/.yapi/agent/models.json` |
| Credentials | `~/.yapi/agent/auth.json` |
| MCP servers | `~/.yapi/agent/mcp.json` and `.yapi/mcp.json` |
| Documentation for the model | `~/.yapi/agent/docs`, installed with yapi |

`/settings` edits the common settings in place and saves them as Pi does.

## Project instructions

yapi reads `AGENTS.md` (or `CLAUDE.md`) from the working folder and each parent folder, and adds them to the system prompt. Use them for build commands, conventions and anything else the agent should know about the project.

## Skills and prompt templates

Skills follow the [Agent Skills specification](https://agentskills.io/specification). Each skill is a folder with a `SKILL.md` file. yapi lists every skill's name and description in the system prompt, and the model reads the full instructions when a task calls for them. `/skill:name` loads one directly.

Prompt templates are Markdown files that become slash commands. Save this as `~/.yapi/agent/prompts/review.md`:

```markdown
---
description: Review staged git changes
argument-hint: "[focus]"
---
Review the staged changes. Focus on ${1:-correctness and error handling}.
```

`/review` then sends the prompt, and `/review concurrency` fills in the focus. Pi's [prompt template documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/prompt-templates.md) lists every argument form.

## Documentation for the model

yapi can explain its own features and look up its docs. When you ask about yapi, the system prompt sends the model to its documentation in `~/.yapi/agent/docs`: these pages, with Pi's documentation and examples in `pi/`. Pi's pages describe the extension API, themes, skills and the other features yapi shares with Pi, under Pi's names. [Coming from Pi](migrating.md) lists the names that change. Without a local copy, the prompt points to this site and to Pi's documentation on GitHub. [Install](install.md#documentation-for-the-model) shows how each way of installing yapi gets the local copy. The install script's `--no-docs` skips the copy, and `YAPI_NO_DOCS=1` keeps the install script and yapi from downloading it.

yapi replaces these files when it is upgraded, so keep your own notes elsewhere.

## Themes

Pick a theme in `/settings` or with `"theme"` in settings. `--use-theme <name>` applies one for a single run. Theme files use Pi's JSON format, described in Pi's [theme documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/themes.md).

## Key bindings

`~/.yapi/agent/keybindings.json` changes the keys. The format and every action are in Pi's [key binding documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/keybindings.md), and `/hotkeys` shows the bindings in effect.

## Images

yapi prepares the images in prompts, `@path` arguments and tool results as Pi does before they reach the model.

- BMP images are converted to PNG.
- With `"images": {"autoResize": true}`, the default, larger images are scaled down to fit 2000×2000 pixels and 4.5 MB, or the limits the model sets. A scaled image may be sent as JPEG, and a note gives the model its original size.
- An image in a prompt or a `read` result that cannot be converted or made to fit is left out, and a note says so.
- `"images": {"blockImages": true}` keeps all images from the model.

Both settings are also in `/settings`.

## Links

In a terminal that shows hyperlinks, yapi writes links as hyperlinks, as Pi does. Markdown links then show only their text, and the paths in `read`, `write`, `edit` and `ls` calls link to their files. In other terminals a Markdown link shows its URL after its text.

yapi knows the hyperlink support of common terminals, such as iTerm2, Ghostty, kitty, WezTerm and VS Code. Inside tmux it asks tmux whether it passes hyperlinks on. Set `"terminal": {"hyperlinks": true}` or `false` in settings to decide yourself. The `PI_HYPERLINKS` environment variable, `1` or `0`, also decides, but the setting wins.

## Project trust

Files in a project's `.yapi` folder can change how yapi behaves and can run code. The first time yapi starts in a project that has them, it asks whether to trust the folder.

- The answer is saved in `~/.yapi/agent/trust.json`, and `/trust` changes it later.
- `--approve` and `--no-approve` decide for one run.
- `"defaultProjectTrust": "always"` in global settings skips the question.

`AGENTS.md` and `CLAUDE.md` load whether or not the project is trusted.

## Security model

yapi reads and edits files and runs commands as your user, so give it the same trust as any program you run in your shell.

- Extensions run in WebAssembly instances with memory and compute limits. In v0.1 every package gets Pi's default grants, which allow file, process, network and environment access, so an extension can do what it could do in Pi. Restricting a package's grants is planned.
- The process grant implies the other three. A process an extension starts runs as your user, outside the sandbox: it reads any file you can, reaches the network and inherits yapi's environment, API keys included. A subagent is such a process, a full yapi with your installed packages. Codemode and any instance without the process grant cannot start one.
- yapi never runs agent sessions inside an extension's instance. A session there would run its tools and use its credentials in yapi's own process, outside the extension's grants.
- MCP servers started over stdio run as normal processes with your permissions. An extension registers a stdio server only with the process grant, and an HTTP server only with the network grant.
- Codemode scripts run in a fresh instance with no file, process or network access. They act only through the tools they call.
- Project trust gates project-local resources. yapi loads nothing from a project's `.yapi` folder or `.agents/skills` until you trust the project.
- Files and tool output that the model reads can steer it. Work in repositories you trust, or run yapi in a container or virtual machine.

Report vulnerabilities privately, as [SECURITY.md](https://github.com/SkymanOne/yapi/blob/main/SECURITY.md) describes.
