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

`/settings` edits the common settings in place and saves them as Pi does.

## Project instructions

yapi reads `AGENTS.md` (or `CLAUDE.md`) from the working folder and each parent folder, and adds them to the system prompt. Use them for build commands, conventions and anything else the agent should know about the project.

## Skills and prompt templates

Skills follow the [Agent Skills specification](https://agentskills.io/specification). Each skill is a folder with a `SKILL.md` file. yapi lists every skill's name and description in the system prompt, and the model reads the full instructions when a task calls for them. `/skill:name` loads one directly. Prompt templates are Markdown files that become slash commands.

## Themes

Pick a theme in `/settings` or with `"theme"` in settings. `--use-theme <name>` applies one for a single run. Theme files use Pi's JSON format.

## Project trust

Files in a project's `.yapi` folder can change how yapi behaves and can run code. The first time yapi starts in a project that has them, it asks whether to trust the folder.

- The answer is saved in `~/.yapi/agent/trust.json`, and `/trust` changes it later.
- `--approve` and `--no-approve` decide for one run.
- `"defaultProjectTrust": "always"` in global settings skips the question.

`AGENTS.md` and `CLAUDE.md` load whether or not the project is trusted.
