# Native extensions in Rust

Native extensions are WebAssembly components written in Rust with the `ri-extension-api` crate. They register tools, commands, flags and event handlers like pi extensions, and use pi's JSON shapes for events and results.

A native extension needs no JavaScript runtime and runs in a WebAssembly instance of its own. A built extension is one `.wasm` file of a few hundred kilobytes.

## Create an extension

Add the `wasm32-wasip2` target to your toolchain:

```sh
rustup target add wasm32-wasip2
```

Create a library crate and depend on the SDK from the ri repository:

```toml
# Cargo.toml
[package]
name = "shout"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
ri-extension-api = { git = "https://github.com/SkymanOne/ri" }
```

Register what the extension offers in an init function and export it with `extension!`:

```rust
// src/lib.rs
use ri_extension_api::{Api, Tool, ToolResult, json, notify};

fn init(api: &mut Api) {
    api.register_tool(Tool::new(
        "shout",
        "Repeats the text in capitals",
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        }),
        |params, _ctx| {
            let text = params["text"].as_str().unwrap_or_default();
            Ok(ToolResult::text(text.to_uppercase()))
        },
    ));
    api.register_command("hello", "Says hello", |args, _ctx| {
        let name = if args.is_empty() { "world" } else { args };
        notify(&format!("Hello, {name}!"), "info");
        Ok(())
    });
}

ri_extension_api::extension!(init);
```

Build it:

```sh
cargo build --release --target wasm32-wasip2
```

The result is `target/wasm32-wasip2/release/shout.wasm`.

## Try it

Load the build for a single run without installing it:

```sh
ri -e target/wasm32-wasip2/release/shout.wasm
```

## Install it

`ri install` takes the `.wasm` file directly:

```sh
ri install ./target/wasm32-wasip2/release/shout.wasm       # for every project
ri install ./target/wasm32-wasip2/release/shout.wasm -l    # for this project only
```

ri records the path in `settings.json` and loads the file from where it is, so a rebuild takes effect the next time ri starts. `ri list` shows the installed extension and `ri remove <path>` uninstalls it. `ri config` turns it off without removing it.

## Share it

A native extension is shared as a package that contains the built `.wasm` file. The simplest layout needs no manifest, because ri loads every `.wasm` file in a package's `extensions` folder:

```text
shout/
└── extensions/
    └── shout.wasm
```

Push that folder to a git repository, and others install it by its URL:

```sh
ri install git:github.com/you/shout
```

To publish on npm, add a `package.json` with a name and a version, then install with `ri install npm:shout`. The manifest can also name the files to load. ri reads the `ri` key before the `pi` key, so one package can ship a native build for ri and a JavaScript build for pi:

```json
{
  "name": "shout",
  "version": "0.1.0",
  "pi": { "extensions": ["./dist/shout.js"] },
  "ri": { "extensions": ["./dist/shout.wasm"] }
}
```

Commit or publish the built `.wasm` file, not only the Rust sources. ri installs prebuilt files and never compiles code during installation, because a build runs `build.rs` scripts and procedural macros outside the sandbox. For the same reason ri skips npm lifecycle scripts.

## API

| Item | Purpose |
|---|---|
| `Api::register_tool` | Offer a tool to the model. `Tool` sets a label, prompt snippet and prompt guidelines. |
| `Api::register_command` | Handle `/name args` |
| `Api::register_flag` | Accept `--name` on the command line, read later with `get_flag` |
| `Api::on` | Handle a pi event such as `session_start` or `tool_call`. The return value is the handler's result in pi, for example `{"block": true, "reason": "..."}`. |
| `notify` | Show a notification |
| `send_message`, `append_entry` | Add a custom message or entry to the session |
| `exec` | Run a process and wait for it |
| `request` | Call any host action by name with a JSON payload |
| `Context` | The mode, the working folder and the rest of pi's `ctx` |

The repository has a complete example in [`guest/examples/hello`](https://github.com/SkymanOne/ri/tree/main/guest/examples/hello), with a tool, a command, a flag and two event handlers.

## Limits

Handlers run synchronously, and host actions answer at once. Asynchronous host operations such as timers and HTTP requests are planned. Extension UI components are available to pi extensions only.
