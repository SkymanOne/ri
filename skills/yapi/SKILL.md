---
name: yapi
description: Run, script and configure yapi, the Rust reimplementation of the Pi coding agent. Use when a task asks you to install yapi, hand work to it from a script or another agent (print, JSON or RPC mode), choose its model or credentials, manage its sessions, packages, skills or settings, move a setup over from Pi, or find out why yapi misbehaves.
license: MIT OR Apache-2.0
---

# Working with yapi

yapi (Yet Another Pi) is a terminal coding agent: give it a goal and a working folder, and it reads files, runs commands and edits code. It reimplements Pi `v1.0.0` in Rust, so it uses Pi's commands, flags, settings, session files, packages and extension API, from one native binary that needs no Node.js.

When you know Pi, assume yapi behaves the same and check [Differences from Pi](https://skymanone.github.io/ri/compat.html) for the exceptions.

## Paths and names

| What | Where |
|---|---|
| Global state | `~/.yapi/agent` (move it with `YAPI_CODING_AGENT_DIR`) |
| Project state | `.yapi/` in the project, read only after the project is trusted |
| Settings | `~/.yapi/agent/settings.json`, overridden by `.yapi/settings.json` |
| Credentials | `~/.yapi/agent/auth.json` |
| Custom providers and models | `~/.yapi/agent/models.json` |
| Sessions | `~/.yapi/agent/sessions/`, one JSONL file per session, grouped by project (move them with `YAPI_CODING_AGENT_SESSION_DIR` or `--session-dir`) |
| Context files | `AGENTS.md` or `CLAUDE.md` in the project, its parents and `~/.yapi/agent` |
| Skills | `~/.yapi/agent/skills`, `.yapi/skills`, `~/.agents/skills` and `.agents/skills` up to the repository root |

Every file uses Pi's format. Provider variables such as `ANTHROPIC_API_KEY` and `PI_OFFLINE` keep Pi's names.

## Install and check

```sh
git clone https://github.com/SkymanOne/ri yapi
cd yapi
cargo install --locked --path crates/yapi
yapi --version
```

This needs a Rust toolchain from rustup. To bring an existing Pi setup across, run `yapi import pi`. It copies settings, credentials, sessions and packages, and keeps files yapi already has.

## Hand a task to yapi

The interactive interface needs a terminal. From a script or another agent, use one of the headless modes.

**Print mode** answers once and exits. Piped input becomes part of the message, and `@path` attaches files and images:

```sh
yapi -p --no-session "Summarize the open TODOs in src/"
git diff | yapi -p --no-session "Write a commit message for this diff"
```

**JSON mode** prints one JSON event per line. To get the final answer, take the last assistant `message_end`:

```sh
yapi --mode json -p --no-session "List the crates in this workspace" \
  | jq -rs '[.[] | select(.type == "message_end" and .message.role == "assistant")]
            | last | .message.content[] | select(.type == "text") | .text'
```

A run emits `session`, `agent_start`, then per turn `turn_start`, `message_start`, `message_update`, `message_end`, `tool_execution_start` and `tool_execution_end`, `turn_end`, and finally `agent_end` with every message.

**RPC mode** keeps yapi running for many prompts. Write one JSON command per line to stdin, such as `{"id":"1","type":"prompt","message":"Run the tests"}`, and read responses (`"type": "response"`, with your `id`) and events from stdout. yapi exits when stdin closes, so keep it open until the run's `agent_end` event arrives. [references/rpc.md](references/rpc.md) has a client and every command.

Flags worth setting when another program drives yapi:

| Flag | Effect |
|---|---|
| `--no-session` | Do not save the run |
| `--model <provider/id[:thinking]>` | Pick the model, such as `anthropic/claude-sonnet-4-5:high` |
| `--tools read,grep,find,ls` | Allow only these tools, here read-only ones. `--no-tools` disables all. |
| `--exclude-tools <names>` | Disable some tools and keep the rest |
| `-ne`, `-ns`, `-nc` | Start without extensions, skills or context files |
| `--offline` | Skip startup network work, as `PI_OFFLINE=1` does |
| `--approve` or `--no-approve` | Trust or ignore project-local files for this run without prompting |
| `--append-system-prompt <text or file>` | Add instructions for this run |

Exit code 0 means the run finished. A provider error ends print mode with a non-zero code and the message on stderr.

## Models and credentials

- API keys come from the environment (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY` and the others `yapi --help` lists), from `auth.json`, or from `--api-key`.
- Subscriptions and accounts (Claude Pro or Max, ChatGPT, GitHub Copilot, Kimi, Meta, xAI, OpenRouter, Radius) sign in with `/login` in the interactive interface. Amazon Bedrock, Google Vertex AI and Cloudflare also read their platform's usual credentials. A local llama.cpp router signs in with `/login llama.cpp` or `LLAMA_BASE_URL`.
- `yapi --list-models [search]` lists what is available with the current credentials.
- `yapi auth check --provider <id>` reports whether a provider is ready, and `--json` makes it machine-readable. `yapi auth print-api-key` and `print-bearer-token` hand a credential to another tool.

Never print or log credentials yourself. Prefer `yapi auth check` over reading `auth.json`.

## Sessions

- `yapi -c` continues the latest session in this folder, `yapi -r` picks one, `--session <path or id>` opens one, and `--fork <path or id>` copies one into a new session.
- In the interface, `/tree` moves to any earlier point and continues from there, `/fork` and `/clone` branch, and `/compact` summarizes old context.
- `yapi --export <session.jsonl> [out.html]` writes a session as HTML.

## Packages, extensions and skills

```sh
yapi install npm:<package>            # a Pi package from npm
yapi install git:github.com/user/repo  # from git
yapi install ./folder -l               # a local package, for this project only
yapi list                              # tags show [npm] or [wasm] extensions
yapi update --extensions
yapi remove <source>
yapi config                            # turn individual resources on and off
yapi -e ./ext.ts                       # load an extension for one run
yapi --skill ./my-skill                # load a skill for one run
```

Pi packages run unchanged. Their extensions run in a WebAssembly sandbox, so code that needs native addons, sockets, worker threads or SQLite fails when it reaches them. To write an extension, use the `yapi-extension` skill.

## Troubleshooting

1. Reproduce with a clean state: `YAPI_CODING_AGENT_DIR=$(mktemp -d) yapi -p --no-session "hi"` rules out settings and packages. `-ne` rules out extensions.
2. `yapi auth check --provider <id>` and `yapi --list-models` confirm the model and credentials.
3. Startup warnings name settings, skills, themes or extensions that failed to load. The interactive header lists the loaded resources, and `--verbose` shows it even when `quietStartup` is set.
4. In the interface, `/debug` writes what yapi rendered and sent to `~/.yapi/agent/yapi-debug.log`.
5. When behavior differs from Pi, check [Differences from Pi](https://skymanone.github.io/ri/compat.html) before treating it as a bug. Report bugs at https://github.com/SkymanOne/ri/issues.
