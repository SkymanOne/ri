# Agent skills

The repository ships two [Agent Skills](https://agentskills.io/specification) that teach a coding agent to work with yapi. They follow the format Pi and yapi load, and other agents that read Agent Skills, such as Claude Code, can use them too.

| Skill | Use it to |
|---|---|
| `yapi` | Install and run yapi, hand it tasks from scripts or other agents in print, JSON or RPC mode, choose models and credentials, and manage sessions, packages and settings. `scripts/ask.py` sends one prompt over RPC and prints the answer. |
| `yapi-extension` | Write, test and package Pi extensions in TypeScript and native extensions in Rust. It has a template for Pi extensions, uses `yapi new` for native ones, notes on where yapi's runtime differs from Pi, and `scripts/check-extension.py`, which loads extensions without a model and reports their commands and load errors. |

## Install them

In yapi or Pi, install the repository at a release tag as a package. Its `skills` folder becomes available in every project:

```sh
yapi install git:github.com/SkymanOne/yapi@v0.1.0
```

To use one skill without the rest of the repository, copy its folder into a skills directory, such as `~/.yapi/agent/skills/`, `~/.agents/skills/` or, for Claude Code, `~/.claude/skills/`:

```sh
cp -r skills/yapi skills/yapi-extension ~/.agents/skills/
```

## Use them

The agent loads a skill when a task matches its description. To load one explicitly in yapi or Pi, run `/skill:yapi` or `/skill:yapi-extension`, followed by the request:

```text
/skill:yapi-extension add a /standup command that summarizes today's git commits
```

The skills' helper scripts need Python 3 and a `yapi` executable on `PATH`, or its path in `--yapi`.
