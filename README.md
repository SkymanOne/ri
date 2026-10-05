<p align="center">
  <a href="https://github.com/SkymanOne/ri/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/SkymanOne/ri/ci.yml?branch=main&style=flat-square&label=CI" /></a>
  <a href="https://skymanone.github.io/ri/"><img alt="Documentation" src="https://img.shields.io/badge/docs-skymanone.github.io%2Fri-blue?style=flat-square" /></a>
  <a href="#license"><img alt="License" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square" /></a>
</p>

# yapi

yapi (Yet Another Pi) is a minimal, extensible agent harness for the terminal, written in Rust. It is a reimplementation of [Pi](https://pi.dev): adapt it to your workflow, not the other way around.

yapi reads Pi's settings, sessions and credentials, and runs Pi packages with their extensions, skills, prompt templates and themes unchanged. Use it interactively, automate it in print, JSON or RPC mode, or extend it with Pi extensions and native extensions written in Rust. It ships as one native binary and needs no Node.js.

> yapi is pre-release software. It follows Pi `v1.0.0`. [Differences from Pi](https://skymanone.github.io/ri/compat.html) lists every known difference.

## Getting started

Install the release binary for Linux or macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/SkymanOne/ri/main/install.sh | sh
```

Or install it with [cargo-binstall](https://github.com/cargo-bins/cargo-binstall), or build it from source with Rust 1.99 or newer:

```bash
cargo binstall --git https://github.com/SkymanOne/ri yapi
cargo install --locked --git https://github.com/SkymanOne/ri yapi
```

[Install](https://skymanone.github.io/ri/install.html) covers each method, downloading the release binaries directly and the platforms they support. The first release is not tagged yet, so for now only the source build works.

Start yapi in the directory where you want it to work:

```bash
cd /path/to/project
yapi
```

For a built-in AI provider, run `/login` inside yapi to connect a subscription or API key. Then give yapi a task. yapi supports Pi's providers, from Claude, ChatGPT and GitHub Copilot subscriptions to Amazon Bedrock, Google Vertex AI and a local llama.cpp server.

If you use Pi already, `yapi import pi` copies your settings, credentials, sessions and packages.

See the [documentation](https://skymanone.github.io/ri/) for full setup and usage instructions.

## How yapi differs from Pi

yapi keeps Pi's commands, file formats and extension API, so most differences are in how extensions run and how fast the harness is.

### Extensions

Pi runs extensions inside its own Node.js process, with the user's permissions. yapi runs every extension in a WebAssembly sandbox:

- **Pi extensions run unchanged.** TypeScript and JavaScript extensions run in a QuickJS-NG runtime compiled to WebAssembly. yapi bundles Pi's packages and shims Node's built-in modules, so no Node.js install is needed.
- **Native extensions are written in Rust.** They build with the `yapi-extension-api` crate for `wasm32-wasip2`, load from the same places as Pi extensions and ship in the same packages.
- **Limits are enforced by the host.** Each call may compute for 60 seconds and each instance may use 1 GiB of memory. A crashed instance restarts without taking yapi down. File, process, network and environment access goes through grants that the host checks. Packages get Pi's defaults, which allow all four, and per-package restrictions are planned.
- **Interfaces never wait for an extension.** Custom components render inside the runtime, and yapi paints their last frame.

What the runtime cannot do: native addons, sockets, threads and SQLite fail when used. Links and images in extension components fall back to text. [Extensions](https://skymanone.github.io/ri/extensions.html) has the details.

Compatibility is measured against Pi itself, with both programs loading the same code:

| Test | Result |
|---|---|
| Pi's example extensions | 79 of 79 register the same tools, commands, flags, shortcuts and event handlers as in Pi |
| The 500 most-downloaded Pi packages on npm | 443 of the 475 that Pi loads in the test sandbox give the same registrations and load errors (93%). Of the 367 whose extensions load in Pi without errors, 349 register the same in yapi (95%). |
| Extension UI | Dialogs, widgets, overlays and custom components match Pi's screens row for row at 80×24 and 120×40 |

`yapi new shout` creates a native extension as a Cargo project that is also a package. Its `src/lib.rs` registers what the extension offers:

```rust
use yapi_extension_api::{Api, Tool, ToolResult, json};

fn init(api: &mut Api) {
    api.register_tool(Tool::new(
        "shout",
        "Repeats the text in capitals",
        json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
        |params, _ctx| {
            let text = params["text"].as_str().unwrap_or_default();
            Ok(ToolResult::text(text.to_uppercase()))
        },
    ));
}

yapi_extension_api::extension!(init);
```

```bash
cd shout
cargo build --release
cp target/wasm32-wasip2/release/shout.wasm extensions/
yapi install .
```

See [Native extensions in Rust](https://skymanone.github.io/ri/native-extensions.html) for the full guide and [the examples](https://skymanone.github.io/ri/native-examples.html) for five complete extensions.

### Performance

yapi is one native executable, so it does not pay for starting Node.js and loading Pi's JavaScript. Measured on the same Linux machine, as medians:

| Measure | yapi | Pi |
|---|---|---|
| `--version` | 2.6 ms | 307.7 ms |
| Interactive first paint | 12.0 ms | 410.9 ms |
| Print mode, start to first request byte | 17.2 ms | 451.1 ms |
| Keystroke to paint, p99, 10,000-line session | 4.7 ms | 11.7 ms |
| Memory, idle | 18.9 MB | 112.5 MB |
| Memory after 20 turns with tool calls | 37.9 MB | 198.2 MB |
| Memory with 57 of Pi's example extensions | 38.3 MB | 116.7 MB |
| Install size | 32.3 MB | 245.2 MB with Node.js |

`cargo xtask bench` produces this table, alternating runs of both programs. [Performance](https://skymanone.github.io/ri/performance.html) explains each measure and the method, and shows the ranges, more memory measures and results from GitHub's hosted Linux and macOS runners.

## Agent skills

The repository ships two [Agent Skills](https://agentskills.io) for coding agents: `yapi`, for running and scripting yapi, and `yapi-extension`, for writing Pi and native extensions. Install both as a package with `yapi install git:github.com/SkymanOne/ri`, or copy them from [skills/](skills/) into any agent's skills folder. [Agent skills](https://skymanone.github.io/ri/agent-skills.html) has the details.

## Development

Clone the repository and run yapi from source:

```bash
git clone https://github.com/SkymanOne/ri yapi
cd yapi
cargo run -p yapi --
```

Before submitting changes, run:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

Read [AGENTS.md](AGENTS.md) for the architecture, design decisions and project rules, and [dev/status.md](dev/status.md) for progress and deferred work. The documentation site is built from [docs/](docs/) with mdBook.

## License

MIT OR Apache-2.0, at your option. Vendored Pi code keeps its MIT notices.
