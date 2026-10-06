<p align="center">
  <a href="https://skymanone.github.io/yapi/">
    <img alt="yapi logo" src="assets/logo.svg" width="224">
  </a>
</p>
<p align="center">
  <strong>Yet Another Pi (in Rust) - Fast, Portable, Compatible</strong>
</p>
<p align="center">
  <a href="https://github.com/SkymanOne/yapi/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/SkymanOne/yapi/ci.yml?branch=main&style=flat-square&label=CI" /></a>
  <a href="https://github.com/SkymanOne/yapi/releases/latest"><img alt="Release" src="https://img.shields.io/github/v/release/SkymanOne/yapi?style=flat-square" /></a>
  <a href="https://skymanone.github.io/yapi/"><img alt="Docs" src="https://img.shields.io/badge/docs-online-blue?style=flat-square" /></a>
  <a href="#license"><img alt="License" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square" /></a>
</p>

# yapi

yapi is an AI coding assistant for your terminal. Open it in a project, describe a task, and it reads files, runs commands and edits code until the task is done. It is a Rust reimplementation of [Pi](https://pi.dev), so it uses Pi's settings, sessions, credentials and packages as they are, and it ships as one native binary that needs no Node.js.

yapi follows Pi `v1.0.0`. [Differences from Pi](https://skymanone.github.io/yapi/compat.html) lists every known difference.

<!-- Terminal screenshot goes here. -->

## Getting started

Install the release binary for Linux or macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/SkymanOne/yapi/main/install.sh | sh
```

Or install it with [cargo-binstall](https://github.com/cargo-bins/cargo-binstall), or build it from source with Rust 1.99 or newer:

```bash
cargo binstall --git https://github.com/SkymanOne/yapi yapi
cargo install --locked --git https://github.com/SkymanOne/yapi --tag v0.1.0 yapi
```

[Install](https://skymanone.github.io/yapi/install.html) covers each method, the supported platforms, upgrading and uninstalling.

Start yapi in the directory where you want it to work:

```bash
cd /path/to/project
yapi
```

Run `/login` inside yapi to connect a subscription or API key, then give yapi a task. yapi supports Pi's providers, from Claude, ChatGPT and GitHub Copilot subscriptions to Amazon Bedrock, Google Vertex AI and a local llama.cpp server.

If you use Pi already, `yapi import pi` copies your settings, credentials, sessions and packages.

## Compatible

yapi keeps Pi's command-line flags, slash commands, file formats, JSON and RPC protocols and extension API. Pi packages run unchanged, with their extensions, skills, prompt templates and themes. Compatibility is measured against Pi itself, with both programs loading the same code:

- Pi's example extensions: 79 of 79 register the same tools, commands, flags, shortcuts and event handlers as in Pi.
- The 500 most-downloaded Pi packages on npm: 418 of 500 work without errors.
- Extension UI: dialogs, widgets, overlays and custom components match Pi's screens row for row at 80×24 and 120×40 in the recorded scenarios, except for the listed differences.

## Fast

yapi does not pay for starting Node.js and loading Pi's JavaScript. Measured on the same Linux machine, as medians:

| Measure | yapi | Pi |
|---|---|---|
| Startup, interactive | 12.0 ms | 410.9 ms |
| Startup, print mode (to first request byte) | 17.2 ms | 451.1 ms |
| Keystroke to paint, p99, 10,000-line session | 4.7 ms | 11.7 ms |
| Memory, idle | 18.9 MB | 112.5 MB |
| Memory after 20 turns with tool calls | 37.9 MB | 198.2 MB |
| Install size | 32.3 MB | 245.2 MB with Node.js |

[Performance](https://skymanone.github.io/yapi/performance.html) explains each measure and the method, with results from GitHub's hosted Linux and macOS runners.

## Portable

yapi is one executable for Linux and macOS on x86_64 and arm64. Extensions run in WebAssembly with memory and compute limits, and in v0.1 they keep Pi's default file, process and network access:

- Pi extensions in TypeScript or JavaScript run in a bundled QuickJS-NG runtime, with no Node.js install.
- Native extensions are written in Rust and compiled to WebAssembly components. `yapi new` creates one, and [Native extensions in Rust](https://skymanone.github.io/yapi/native-extensions.html) is the guide.

Native addons, sockets, threads and SQLite are not available to extensions. [Extensions](https://skymanone.github.io/yapi/extensions.html) has the details.

## Agent skills

The repository ships two [Agent Skills](https://agentskills.io) for coding agents: `yapi`, for running and scripting yapi, and `yapi-extension`, for writing Pi and native extensions. Install both with `yapi install git:github.com/SkymanOne/yapi`. [Agent skills](https://skymanone.github.io/yapi/agent-skills.html) has the details.

## Documentation

The [documentation](https://skymanone.github.io/yapi/) covers models and sign-in, interactive use, sessions, automation, configuration, extensions, packages, MCP servers and troubleshooting. Report problems in the [issue tracker](https://github.com/SkymanOne/yapi/issues).

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) explains how to build yapi, run its checks and find your way around the code.

## License

MIT OR Apache-2.0, at your option. Vendored Pi code keeps its MIT notices.
