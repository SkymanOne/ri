# ri

**ri is a terminal coding agent written in Rust.** It reads, edits and runs your code with the model of your choice, and it is a drop-in companion to [pi](https://github.com/earendil-works/pi) `v1.0.0`: the same commands, settings, sessions, packages and extensions, in a single native binary.

```text
$ ri
 █▀ ▀ v0.1.0
 █  █ escape interrupt · ctrl+c/ctrl+d clear/exit · / commands · ! bash · ctrl+o more

 > Add a --verbose flag to the CLI and update the tests
```

- **Familiar.** pi's CLI flags, slash commands, key bindings, JSON and RPC protocols, session files and extension API. Your pi setup moves over with one command.
- **Fast and small.** It starts in about 2 ms and idles at about 15 MB, against pi's 250 ms and 110 MB ([numbers](#performance)).
- **No Node needed.** pi packages install from npm, git or a local folder, and their TypeScript extensions run inside a sandboxed WebAssembly runtime.

> ri is pre-release software. What works, what is missing, and where ri intentionally differs from pi are listed in [docs/status.md](docs/status.md) and [docs/compat.md](docs/compat.md).

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Models and sign-in](#models-and-sign-in)
- [Working in the terminal](#working-in-the-terminal)
- [Scripting: print, JSON and RPC modes](#scripting-print-json-and-rpc-modes)
- [Sessions](#sessions)
- [Configuration](#configuration)
- [Packages and extensions](#packages-and-extensions)
- [MCP servers and codemode](#mcp-servers-and-codemode)
- [Coming from pi](#coming-from-pi)
- [Performance](#performance)
- [Troubleshooting](#troubleshooting)
- [Development](#development)

## Install

ri builds with stable Rust. Install Rust with [rustup](https://rustup.rs), then build ri from a clone; the pinned toolchain in `rust-toolchain.toml` is installed automatically:

```sh
git clone https://github.com/SkymanOne/ri
cd ri
cargo install --locked --path crates/ri
```

This puts `ri` in `~/.cargo/bin`. Check it with `ri --version`.

Tagged releases publish ready-made binaries for Linux and macOS on x86_64 and arm64, each with a `.sha256` checksum. No release is tagged yet.

## Quick start

1. Give ri a model. The quickest way is an API key in the environment, for example:

   ```sh
   export ANTHROPIC_API_KEY=sk-ant-...
   ```

   Any provider in [Models and sign-in](#models-and-sign-in) works, as do Claude, ChatGPT and GitHub Copilot subscriptions.

2. Start ri in your project:

   ```sh
   cd ~/code/my-project
   ri
   ```

3. Type what you want and press <kbd>Enter</kbd>. ri reads files, runs commands and edits code with its `read`, `bash`, `edit` and `write` tools, and shows each step as it goes. Press <kbd>Esc</kbd> to interrupt, and <kbd>Ctrl</kbd>+<kbd>C</kbd> twice to quit.

Some first things to try:

```text
Explain how requests flow through src/server.rs
Find why `cargo test` fails and fix it
@Cargo.toml Which dependencies are outdated?
!git status
```

`@path` attaches a file, and `!command` runs a shell command yourself and shows its output to the model.

## Models and sign-in

**API keys.** ri reads the same environment variables as pi. Common ones:

| Provider | Variable |
|---|---|
| Anthropic | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Google Gemini | `GEMINI_API_KEY` |
| OpenRouter | `OPENROUTER_API_KEY` |
| Groq | `GROQ_API_KEY` |
| xAI | `XAI_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| Mistral | `MISTRAL_API_KEY` |
| OpenCode Zen and Go | `OPENCODE_API_KEY` |

`ri --help` lists every provider, including Azure OpenAI, Amazon Bedrock, Cloudflare and Vercel AI Gateway. You can also run `/login` inside ri to save a key to `~/.ri/agent/auth.json`.

**Subscriptions.** Run `/login` and pick Claude Pro/Max, ChatGPT Plus/Pro (Codex) or GitHub Copilot. ri opens the browser sign-in and refreshes the token for you.

**Choosing a model.**

```sh
ri --list-models sonnet            # search the catalog
ri --model anthropic/claude-sonnet-4-5
ri --model sonnet:high             # a pattern, with a thinking level
ri --models "sonnet,gpt-5*"        # the models Ctrl+P cycles through
```

Inside ri, `/model` (or <kbd>Ctrl</kbd>+<kbd>L</kbd>) opens the model picker, <kbd>Ctrl</kbd>+<kbd>P</kbd> cycles models, and <kbd>Shift</kbd>+<kbd>Tab</kbd> cycles the thinking level. Custom providers and models go in `~/.ri/agent/models.json`, in pi's format.

**For other tools.** `ri auth` hands credentials to scripts without starting a session:

```sh
ri auth check --provider anthropic --json      # {"status":"ready","provider":"anthropic","authType":"api_key"}
ri auth print-api-key --provider openai
ri auth print-bearer-token --provider openai-codex --min-expiry 30m
```

## Working in the terminal

The editor at the bottom takes your message. These keys are the defaults; `/hotkeys` shows the full list as configured.

| Key | Action |
|---|---|
| <kbd>Enter</kbd> | Send. While ri works, it steers the current turn. |
| <kbd>Alt</kbd>+<kbd>Enter</kbd> | Queue a follow-up for when ri finishes |
| <kbd>Shift</kbd>+<kbd>Enter</kbd> | New line |
| <kbd>Esc</kbd> | Interrupt. Twice on an empty editor opens the session tree. |
| <kbd>Ctrl</kbd>+<kbd>C</kbd> | Clear the editor; twice to quit |
| <kbd>Ctrl</kbd>+<kbd>D</kbd> | Quit when the editor is empty |
| <kbd>Ctrl</kbd>+<kbd>O</kbd> | Expand or collapse tool output |
| <kbd>Ctrl</kbd>+<kbd>T</kbd> | Show or hide thinking |
| <kbd>Ctrl</kbd>+<kbd>G</kbd> | Write the message in `$EDITOR` |
| <kbd>Tab</kbd> | Complete paths and commands |
| <kbd>↑</kbd> | Previous messages |

Type `/` to see the commands. The ones you will use most:

| Command | What it does |
|---|---|
| `/model`, `/thinking` | Switch model or thinking level |
| `/settings` | Change settings such as the theme, auto-compaction or fullscreen mode |
| `/scoped-models` | Choose which models <kbd>Ctrl</kbd>+<kbd>P</kbd> cycles through |
| `/new`, `/resume` | Start a new session, or open an earlier one |
| `/tree` | Browse the session's branches and jump to any earlier point |
| `/fork`, `/clone` | Branch from an earlier message, or duplicate the session |
| `/compact` | Summarize older context to free up the window |
| `/name`, `/session` | Name the session; show its file, tokens and cost |
| `/copy`, `/export` | Copy the last answer; save the session to a file |
| `/login`, `/logout` | Manage provider credentials |
| `/reload` | Reload settings, keybindings, extensions, skills and themes |
| `/hotkeys` | Show every key binding |

Prompt templates, skills (`/skill:name`) and extension commands appear in the same list.

## Scripting: print, JSON and RPC modes

```sh
ri -p "Summarize the open TODOs in src/"            # print the answer and exit
git diff | ri -p "Write a commit message for this diff"
ri -p @report.md @chart.png "What does the chart show?"
ri --mode json -p "List the crates in this workspace"  # one JSON event per line
ri --mode rpc                                        # commands on stdin, events on stdout
```

Piped input becomes part of the first message. `--no-session` keeps a run out of your history, and `--tools read,grep,find,ls` limits ri to read-only tools. JSON and RPC modes speak pi's protocols, so pi's clients drive ri unchanged.

## Sessions

Every conversation is saved as a JSONL file under `~/.ri/agent/sessions/`, grouped by project, in pi's session format.

```sh
ri -c                      # continue the last session in this project
ri -r                      # pick a session to resume
ri --session 3f2a          # open a session by file or id prefix
ri --fork 3f2a             # branch an existing session into a new one
ri --no-session            # do not save this run
```

Sessions are trees: going back with `/tree` or `/fork` keeps the abandoned branch, so nothing is lost.

## Configuration

ri keeps global state in `~/.ri/agent` (set `RI_CODING_AGENT_DIR` to move it) and project state in a `.ri` folder, both in pi's formats.

| What | Where |
|---|---|
| Settings | `~/.ri/agent/settings.json`, overridden by `.ri/settings.json` |
| Project instructions | `AGENTS.md` or `CLAUDE.md`, in the project and its parent folders, and in `~/.ri/agent` |
| Custom system prompt | `SYSTEM.md` replaces it, `APPEND_SYSTEM.md` extends it (`.ri/` or `~/.ri/agent/`) |
| Skills | `~/.ri/agent/skills`, `~/.agents/skills`, `.ri/skills` and `.agents/skills` |
| Prompt templates | `~/.ri/agent/prompts` and `.ri/prompts` |
| Themes | `~/.ri/agent/themes` and `.ri/themes`; pick one with `"theme"` in settings or `--use-theme` |
| Key bindings | `~/.ri/agent/keybindings.json` |
| Models and providers | `~/.ri/agent/models.json` |
| MCP servers | `~/.ri/agent/mcp.json` and `.ri/mcp.json` |

**Project trust.** Files in a project's `.ri` folder can change how ri behaves and can run code, so the first time ri starts in a project that has them it asks whether to trust the folder. Your answer is saved in `~/.ri/agent/trust.json`; `/trust` changes it later. `--approve` and `--no-approve` decide for one run, and `"defaultProjectTrust": "always"` in global settings skips the question. `AGENTS.md` and `CLAUDE.md` always load.

## Packages and extensions

pi packages bundle extensions, skills, prompt templates and themes. ri installs them without Node:

```sh
ri install npm:@scope/some-pi-package     # from npm
ri install git:github.com/user/repo       # from git
ri install ./my-package -l                 # a local folder, for this project only
ri list
ri update --extensions
ri remove npm:@scope/some-pi-package
```

**Turning resources on and off.** `ri config` lists every extension, skill, prompt template and theme that your packages, settings and the agent and project folders provide. <kbd>Space</kbd> toggles one, and the choice is saved as a pattern in `settings.json`. <kbd>Tab</kbd> switches between your global settings and overrides for the current project; `ri config -l` starts with the project.

**Writing an extension.** Extensions use pi's API unchanged. Save this as `~/.ri/agent/extensions/hello.ts`:

```typescript
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  pi.registerCommand("hello", {
    description: "Show a greeting",
    handler: async (name, ctx) => {
      ctx.ui.notify(`Hello, ${name || "world"}!`, "info");
    },
  });
}
```

Start ri and run `/hello Ada`. While developing, load a file for one run with `ri -e ./hello.ts`, or a whole package folder with `ri -e ./my-package`, which brings its skills, prompts and themes too. pi's [extension documentation](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/extensions.md) covers tools, events, UI components and more; all of it applies to ri.

**How extensions run.** TypeScript and JavaScript extensions run in a QuickJS runtime compiled to WebAssembly, with Node's built-in modules provided by shims. Native extensions are WebAssembly components written in Rust with [`guest/ri-extension-api`](guest/ri-extension-api); a `.wasm` file loads wherever an extension file does. ri checks every file, process, network and environment access an extension makes against its grants. Today every package gets pi's defaults, which allow all four; per-package restrictions in settings are planned.

## MCP servers and codemode

ri connects to [MCP](https://modelcontextprotocol.io) servers over stdio and streamable HTTP. Describe them in `~/.ri/agent/mcp.json`:

```json
{
  "mcpServers": {
    "filesystem": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
    "docs": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" } }
  }
}
```

`ri mcp` edits that file and checks servers without starting a session:

```sh
ri mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem .
ri mcp add docs --url https://example.com/mcp --bearer-token-env-var DOCS_TOKEN
ri mcp list                                # state, tools and errors; exits 1 if a server fails
ri mcp remove docs
```

Inside ri, `/mcp` shows the same status. By default, MCP tools are offered through **codemode**: instead of one tool call per step, the model writes a short script that calls several tools at once. Scripts run in a sandbox with no file, process or network access of their own. Set `"exposure": "direct"` on a server to offer its tools directly instead, or enable codemode for every tool with `--tools read,bash,edit,write,codemode`.

## Coming from pi

```sh
ri import pi
```

copies pi's settings, credentials, models, key bindings, MCP servers, trust decisions, sessions, prompts, skills, themes, extensions and packages from `~/.pi/agent`, and the current project's `.pi` folder, into ri's. Files ri already has are kept. After that, ri and pi can be used side by side; session files written by either open in the other.

Environment variables that name pi's directories become `RI_CODING_AGENT_DIR` and `RI_CODING_AGENT_SESSION_DIR`; provider keys and `PI_OFFLINE` keep their names.

## Performance

Measured with `cargo xtask bench --pi`, which runs both programs on the same machine. Linux is an x86_64 container; macOS is GitHub's hosted arm64 runner.

| | Budget | ri, Linux | pi, Linux | ri, macOS | pi, macOS |
|---|---|---|---|---|---|
| `--version` | < 5 ms | 2.0 ms | 246 ms | 5.8 ms | 204 ms |
| Print mode, start to first request byte | < 25 ms | 13.1 ms | 336 ms | 10.8 ms | 323 ms |
| Interactive first paint | < 40 ms | 10.0 ms | 330 ms | 22.7 ms | 270 ms |
| Keystroke to paint, p99 | < 16 ms | 1.9 ms | 6.1 ms | 6.7 ms | 9.7 ms |
| Idle memory | < 30 MB | 15.5 MiB | 106 MiB | 14.4 MiB | 119 MiB |
| Idle memory, 10 extensions | < 70 MB | 28.6 MiB | 110 MiB | 26.2 MiB | 124 MiB |
| Stripped binary | < 35 MB | 33.6 MB | | 26.5 MB | |

On macOS, `--version` is over budget because loading the system's Security framework costs about 2 ms; see [docs/status.md](docs/status.md#m7-codemode-budgets-import-release).

## Troubleshooting

- **Behind a proxy.** ri honors `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY`, and the `httpProxy` setting. Certificates are checked against the operating system's trust store.
- **Offline.** `--offline` (or `PI_OFFLINE=1`) skips startup network work such as downloading `fd` and `rg`.
- **An extension breaks startup.** `ri -ne` starts without extensions.
- **Something looks wrong.** `/debug` writes what ri rendered and sent to `~/.ri/agent/ri-debug.log`. Please include it in [an issue](https://github.com/SkymanOne/ri/issues).

## Development

ri is a Cargo workspace whose crates mirror pi's packages. [AGENTS.md](AGENTS.md) describes the architecture, the design decisions and the checks every change passes:

```sh
cargo test --workspace                 # unit tests and the scenario suite, recorded from pi
cargo xtask e2e --differential         # run pi and ri side by side on every scenario
cargo xtask bench --pi <path-to-pi>    # the performance budgets
```

## License

MIT or Apache-2.0, at your option. Vendored pi code keeps its MIT notices.
