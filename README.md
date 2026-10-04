<p align="center">
  <a href="https://github.com/SkymanOne/ri/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/SkymanOne/ri/ci.yml?branch=main&style=flat-square&label=CI" /></a>
  <a href="https://skymanone.github.io/ri/"><img alt="Documentation" src="https://img.shields.io/badge/docs-skymanone.github.io%2Fri-blue?style=flat-square" /></a>
  <a href="#license"><img alt="License" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square" /></a>
</p>

# ri

ri is a Rust implementation of [pi](https://github.com/earendil-works/pi), a minimal, extensible AI agent for the terminal. It runs pi packages and reads pi's settings, sessions and credentials unchanged, from one native binary that needs no Node.js.

Use ri interactively, automate it in print, JSON or RPC mode, or extend it with pi extensions and native extensions written in Rust.

> ri is pre-release software. It follows pi `v1.0.0`, and [Differences from pi](https://skymanone.github.io/ri/compat.html) lists every intentional difference.

## Getting started

Build and install the command-line interface with Cargo:

```bash
git clone https://github.com/SkymanOne/ri
cd ri
cargo install --locked --path crates/ri
```

This requires a Rust toolchain from [rustup](https://rustup.rs). The version pinned in `rust-toolchain.toml` installs on the first build.

Start ri in the directory where you want it to work:

```bash
cd /path/to/project
ri
```

Run `/login` inside ri to connect a subscription or API key. Then give ri a task.

If you use pi already, `ri import pi` copies your settings, credentials, sessions and packages.

See the [documentation](https://skymanone.github.io/ri/) for full setup and usage instructions.

## Models and sign-in

ri reads the same API key variables as pi, such as `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` and `GEMINI_API_KEY`. `ri --help` lists every provider.

`/login` signs in with a Claude Pro or Max, ChatGPT Plus or Pro, or GitHub Copilot subscription, or stores an API key in `~/.ri/agent/auth.json`. [Models and sign-in](https://skymanone.github.io/ri/models.html) covers model selection, custom providers and `ri auth`.

## Extensions

ri runs pi extensions unchanged and adds native extensions written in Rust. Both run in WebAssembly sandboxes on one host, load from the same places and ship in the same packages.

Compatibility is measured against pi itself:

| Test | Result |
|---|---|
| pi's example extensions | 79 of 79 register the same tools, commands, flags and shortcuts as in pi |
| The 500 most-downloaded pi packages on npm | 443 of 475 comparable packages install and register as in pi (93%) |
| Extension UI | Dialogs, widgets, overlays and custom components match pi's screens row for row |

pi extensions run in a QuickJS-NG runtime compiled to WebAssembly. ri bundles pi's packages and shims Node's built-in modules, so no Node.js install is needed.

Native extensions build with the `ri-extension-api` crate for `wasm32-wasip2`:

```rust
use ri_extension_api::{Api, Tool, ToolResult, json};

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

ri_extension_api::extension!(init);
```

Build it and install the result:

```bash
cargo build --release --target wasm32-wasip2
ri install ./target/wasm32-wasip2/release/shout.wasm
```

To share it, put the `.wasm` file in an `extensions` folder of a git repository or npm package. Others then install it like any pi package, with `ri install git:github.com/you/shout` or `ri install npm:shout`. See [Native extensions in Rust](https://skymanone.github.io/ri/native-extensions.html) for the full guide and [the examples](https://skymanone.github.io/ri/native-examples.html) for five complete extensions.

## Performance

| Measure | ri | pi |
|---|---|---|
| `--version` | 2.0 ms | 246 ms |
| Interactive first paint | 10.0 ms | 330 ms |
| Idle memory | 15.5 MiB | 106 MiB |

Measured on Linux x86_64 with `cargo xtask bench`. [Performance](https://skymanone.github.io/ri/performance.html) has every budget and the macOS results.

## Development

Clone the repository and run ri from source:

```bash
git clone https://github.com/SkymanOne/ri
cd ri
cargo run -p ri --
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

MIT OR Apache-2.0, at your option. Vendored pi code keeps its MIT notices.
