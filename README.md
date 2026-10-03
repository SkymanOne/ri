# ri

ri is a coding agent compatible with [pi](https://github.com/earendil-works/pi) `v1.0.0`, written in Rust. It reads pi's settings, credentials, models and sessions unchanged, runs pi packages and extensions, and adds sandboxed extensions built as WebAssembly components.

- **Compatible.** pi's CLI flags, slash commands, JSON and RPC protocols, session files and extension API. pi packages install from npm, git or a local path and run without Node.
- **Light.** A single native binary that starts in milliseconds and idles in tens of megabytes.
- **Sandboxed.** Every extension runs in a WebAssembly runtime with grants for files, processes, network and environment.

Intentional differences from pi are listed in [docs/compat.md](docs/compat.md); progress and known gaps are in [docs/status.md](docs/status.md).

## Install

Download the archive for your platform from the [releases](https://github.com/SkymanOne/ri/releases), check it against its `.sha256` file, and put `ri` on your `PATH`:

```sh
tar -xzf ri-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
install ri-v0.1.0-x86_64-unknown-linux-gnu/ri ~/.local/bin/
```

Builds are published for Linux and macOS on x86_64 and arm64. To build from source, install Rust with [rustup](https://rustup.rs) and run:

```sh
cargo install --locked --git https://github.com/SkymanOne/ri ri
```

## Use

ri keeps its state in `~/.ri/agent` (`RI_CODING_AGENT_DIR` overrides it) and in a project's `.ri` directory, in pi's formats. To bring over an existing pi setup:

```sh
ri import pi
```

This copies pi's settings, credentials, models, keybindings, MCP servers, sessions, prompts, skills, themes, extensions and packages. Files ri already has are kept.

Everything else works as in pi:

```sh
ri                                   # interactive session
ri -p "Summarize README.md"          # print mode
ri --mode rpc                        # RPC over stdin and stdout
ri install npm:<package>             # install a pi package
ri -e ./my-extension.ts              # load an extension for one run
```

Sign in with `/login` for Claude Pro/Max, ChatGPT or GitHub Copilot subscriptions, or set an API key in the environment, as with pi.

## Extensions

pi extensions in TypeScript or JavaScript load as they are. They run in `ri-js`, a QuickJS runtime compiled to WebAssembly, with Node's built-in modules provided by shims. Native extensions are WebAssembly components written in Rust with the SDK in [`guest/ri-extension-api`](guest/ri-extension-api); a `.wasm` file loads wherever an extension file does.

## Development

[AGENTS.md](AGENTS.md) describes the architecture, the design decisions and the checks every change passes.

## License

MIT or Apache-2.0, at your option. Vendored pi code keeps its MIT notices.
