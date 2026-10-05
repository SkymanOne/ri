# Native extensions in Rust

Native extensions are WebAssembly components written in Rust with the `yapi-extension-api` crate. They register tools, commands, flags and event handlers like Pi extensions, and use Pi's JSON shapes for events and results.

A native extension needs no JavaScript runtime and runs in a WebAssembly instance of its own. A built extension is one `.wasm` file of a few hundred kilobytes.

## Create an extension

Add the `wasm32-wasip2` target to your toolchain once:

```sh
rustup target add wasm32-wasip2
```

Create a project:

```sh
yapi new shout
```

`yapi new` writes a Cargo project that is also a yapi package:

```text
shout/
├── .cargo/config.toml   makes wasm32-wasip2 the default target
├── Cargo.toml           a cdylib crate that depends on yapi-extension-api
├── README.md
├── extensions/          where the built extension goes
├── package.json         names extensions/shout.wasm for yapi
└── src/lib.rs           the hello example, to replace with your own
```

The package takes the directory's name, and `--name` sets another. The same template works with [cargo-generate](https://github.com/cargo-generate/cargo-generate), which also asks for the name:

```sh
cargo generate --git https://github.com/SkymanOne/ri crates/yapi/templates/extension
```

To start without the template, create a library crate with `crate-type = ["cdylib"]` and depend on the SDK from the yapi repository:

```toml
[dependencies]
yapi-extension-api = { git = "https://github.com/SkymanOne/ri" }
```

An extension registers what it offers in an init function and exports it with `extension!`:

```rust
// src/lib.rs
use yapi_extension_api::{Api, Tool, ToolResult, json, notify};

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

yapi_extension_api::extension!(init);
```

Build it, and copy the build into the package:

```sh
cargo build --release
cp target/wasm32-wasip2/release/shout.wasm extensions/
```

A crate without the template's `.cargo/config.toml` builds with `cargo build --release --target wasm32-wasip2`.

## Try it

Load the package for a single run without installing it:

```sh
yapi -e .
```

`-e` also takes the `.wasm` file itself.

## Install it

`yapi install` takes the package folder or the `.wasm` file:

```sh
yapi install .       # for every project
yapi install . -l    # for this project only
```

yapi records the path in `settings.json` and loads the file from where it is, so a rebuild takes effect the next time yapi starts. `yapi list` shows the installed extension and `yapi remove <path>` uninstalls it. `yapi config` turns it off without removing it.

## Share it

A native extension is shared as a package that contains the built `.wasm` file. A project from `yapi new` already is one: commit `extensions/shout.wasm` with the sources, push the repository, and others install it by its URL:

```sh
yapi install git:github.com/you/shout
```

To publish on npm, run `npm publish`, then install with `yapi install npm:shout`. The template's `package.json` publishes only the `extensions` folder.

A package needs no manifest, because yapi loads every `.wasm` file in a package's `extensions` folder. A manifest names the files, and yapi reads its `yapi` key before the `pi` key, so one package can ship a native build for yapi and a JavaScript build for Pi:

```json
{
  "name": "shout",
  "version": "0.1.0",
  "pi": { "extensions": ["./dist/shout.js"] },
  "yapi": { "extensions": ["./dist/shout.wasm"] }
}
```

Commit or publish the built `.wasm` file, not only the Rust sources. yapi installs prebuilt files and never compiles code during installation, because a build runs `build.rs` scripts and procedural macros outside the sandbox. For the same reason yapi skips npm lifecycle scripts.

## API

| Item | Purpose |
|---|---|
| `Api::register_tool` | Offer a tool to the model. `Tool` sets a label, prompt snippet and prompt guidelines. |
| `Api::register_command` | Handle `/name args` |
| `Api::register_flag` | Accept `--name` on the command line, read later with `get_flag` |
| `Api::on` | Handle a Pi event such as `session_start` or `tool_call`. The return value is the handler's result in Pi, for example `{"block": true, "reason": "..."}`. |
| `notify` | Show a notification |
| `send_message`, `append_entry` | Add a custom message or entry to the session |
| `exec` | Run a process and wait for it |
| `request` | Call any host action by name with a JSON payload |
| `Context` | The mode, the working folder and the rest of Pi's `ctx` |

[Native extension examples](native-examples.md) walks through five complete extensions: a guard for dangerous commands, protected paths, a todo list kept per session branch, a git status reporter and a minimal starting point.

## Limits

Handlers run synchronously, and host actions answer at once. Asynchronous host operations such as timers and HTTP requests are planned. Extension UI components are available to Pi extensions only.
