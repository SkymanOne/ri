# Configuration

ri keeps global state in `~/.ri/agent` and project state in a `.ri` folder. Set `RI_CODING_AGENT_DIR` to move the global folder. Every file uses pi's format, described in pi's [settings documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/settings.md).

| What | Where |
|---|---|
| Settings | `~/.ri/agent/settings.json`, overridden by `.ri/settings.json` |
| Project instructions | `AGENTS.md` or `CLAUDE.md` in the project, its parent folders and `~/.ri/agent` |
| System prompt | `SYSTEM.md` replaces it and `APPEND_SYSTEM.md` extends it, in `.ri/` or `~/.ri/agent/` |
| Skills | `~/.ri/agent/skills`, `~/.agents/skills`, `.ri/skills` and `.agents/skills` |
| Prompt templates | `~/.ri/agent/prompts` and `.ri/prompts` |
| Themes | `~/.ri/agent/themes` and `.ri/themes` |
| Key bindings | `~/.ri/agent/keybindings.json` |
| Models and providers | `~/.ri/agent/models.json` |
| Credentials | `~/.ri/agent/auth.json` |
| MCP servers | `~/.ri/agent/mcp.json` and `.ri/mcp.json` |

`/settings` edits the common settings in place and saves them as pi does.

## Project instructions

ri reads `AGENTS.md` (or `CLAUDE.md`) from the working folder and each parent folder, and adds them to the system prompt. Use them for build commands, conventions and anything else the agent should know about the project.

## Skills and prompt templates

Skills follow the [Agent Skills specification](https://agentskills.io/specification). Each skill is a folder with a `SKILL.md` file. ri lists every skill's name and description in the system prompt, and the model reads the full instructions when a task calls for them. `/skill:name` loads one directly. Prompt templates are Markdown files that become slash commands.

## Themes

Pick a theme in `/settings` or with `"theme"` in settings. `--use-theme <name>` applies one for a single run. Theme files use pi's JSON format.

## Project trust

Files in a project's `.ri` folder can change how ri behaves and can run code. The first time ri starts in a project that has them, it asks whether to trust the folder.

- The answer is saved in `~/.ri/agent/trust.json`, and `/trust` changes it later.
- `--approve` and `--no-approve` decide for one run.
- `"defaultProjectTrust": "always"` in global settings skips the question.

`AGENTS.md` and `CLAUDE.md` load whether or not the project is trusted.
