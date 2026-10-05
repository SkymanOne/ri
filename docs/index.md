# yapi

yapi (Yet Another Pi) is a minimal, extensible AI agent for the terminal, written in Rust. Give it a goal and a working folder, and it can inspect files, run commands, edit content and work through multi-step tasks.

yapi is a reimplementation of [Pi](https://pi.dev) `v1.0.0`. It uses Pi's commands, settings, session files, packages and extension API, so a Pi setup works in yapi as it is. Adapt it to your workflow with extensions, skills, prompt templates and themes, or install a Pi package. It ships as one native binary and needs no Node.js.

## Start using yapi

New to yapi? [Install it](install.md) and follow the [Quickstart](quickstart.md) to connect a model and complete your first task. Using Pi already? [Coming from Pi](migrating.md) shows how to bring your setup over.

- [Use yapi interactively](interactive.md) to add files, run commands and direct ongoing work.
- [Choose a model](models.md) or connect a subscription, API key, cloud provider or local model.
- [Continue or branch a session](sessions.md) to resume work or explore another approach without losing history.
- [Configure yapi](configuration.md) for your preferences, instructions and reusable resources.

## Customize yapi

- [Extensions](extensions.md) add tools, commands, event handlers and interface components. Pi extensions run unchanged.
- [Native extensions](native-extensions.md) do the same in Rust, compiled to WebAssembly.
- [Packages](packages.md) bundle extensions, skills, prompt templates and themes, and install from npm, git or a local path.
- [MCP servers and codemode](mcp.md) connect external tools.

## Automate yapi

Use [print, JSON and RPC modes](automation.md) for scripts, editors and other programs. The output formats and protocols are Pi's, so Pi's clients work with yapi.

## How yapi differs from Pi

| | Pi | yapi |
|---|---|---|
| Runtime | Node.js 22.19 or newer | One native binary |
| Extensions | Run in Pi's process with the user's permissions | Run in WebAssembly sandboxes with memory and compute limits. Pi extensions run unchanged, and native extensions are written in Rust. |
| Startup and memory | 308 ms to first paint, 115 MB idle, 245 MB installed with Node.js | 9 ms to first paint, 20 MB idle, a 32 MB executable, on the same machine. See [Performance](performance.md). |

[Differences from Pi](compat.md) lists every known difference, including the Pi features not ported yet.

## Compatibility

yapi is tested against Pi itself. Every check below runs both programs on the same input.

| Check | Result |
|---|---|
| End-to-end scenarios | 245 of 245 match Pi: 97 terminal screens, 51 JSON event streams, 77 CLI and print mode runs, 19 RPC sessions and Pi's own RPC client |
| Settings, credentials and session files | Read and written back byte for byte |
| Pi's example extensions | 79 of 79 register the same tools, commands, flags and shortcuts as in Pi |
| The 500 most-downloaded Pi packages on npm | 443 of the 475 that Pi loads in the test sandbox behave the same (93%). [Extensions](extensions.md#compatibility-with-pi-extensions) has the breakdown. |

yapi is pre-release software. Report problems in the [issue tracker](https://github.com/SkymanOne/ri/issues).
