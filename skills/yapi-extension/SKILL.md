---
name: yapi-extension
description: Write, test and package extensions for yapi, the Rust reimplementation of the Pi coding agent, either as Pi extensions in TypeScript that also run in Pi, or as native WebAssembly extensions in Rust. Use when asked to add a tool, slash command, flag, event handler or interface component to yapi, to port a Pi extension to yapi, or to find out why an extension fails to load in yapi.
license: MIT OR Apache-2.0
---

# Writing yapi extensions

yapi runs two kinds of extensions, and both register tools, commands, flags and event handlers through Pi's extension model:

| | Pi extension | Native extension |
|---|---|---|
| Language | TypeScript or JavaScript | Rust, compiled to `wasm32-wasip2` |
| Runs in | yapi and Pi, unchanged | yapi only |
| API | Pi's `ExtensionAPI` from `@earendil-works/pi-coding-agent` 1.0 | The `yapi-extension-api` crate |
| Interface components | Yes, pi-tui's | Yes, the SDK's `Component` trait, and yapi's ports of pi-tui's widgets with the `widgets` feature |
| npm dependencies | Yes | No |
| Dialogs, processes and background work | Yes | Yes, from `async` handlers |
| Sandbox | A shared QuickJS-NG runtime in WebAssembly, with Node shims | A WebAssembly instance of its own |

Choose a Pi extension unless the user asks for Rust or a sandboxed, self-contained `.wasm` file. Both kinds load from the same places and ship in the same packages.

## Pi extension

1. Start from [templates/pi-extension.ts](templates/pi-extension.ts), which registers a flag, a tool, a command and a `tool_call` handler. Save it as `extensions/<name>.ts` in a package, `.yapi/extensions/<name>.ts` in a project, or `~/.yapi/agent/extensions/<name>.ts` for the user.
2. Write against Pi's API. The default export receives `pi: ExtensionAPI`:
   - `pi.registerTool({ name, label, description, parameters, execute })` with parameters from `typebox` (`Type.Object(...)`). `execute(toolCallId, params, signal, onUpdate, ctx)` returns `{ content: [{ type: "text", text }], details }`.
   - `pi.registerCommand(name, { description, handler: async (args, ctx) => ... })` handles `/name args`.
   - `pi.registerFlag(name, { type, default, description })`, read with `pi.getFlag(name)`.
   - `pi.on(event, handler)` for events such as `session_start`, `input`, `before_agent_start`, `tool_call` (return `{ block: true, reason }` to stop a call) and `tool_result`.
   - `ctx.ui` has `notify`, `select`, `confirm`, `input`, `setStatus`, widgets and custom components. Check `ctx.hasUI` before opening dialogs, because print and JSON modes have no interface.
   - `pi.sendMessage`, `pi.sendUserMessage`, `pi.appendEntry` and `pi.exec` act on the session and the system.
   Pi's [extension documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/extensions.md) and its [examples](https://github.com/earendil-works/pi/tree/v1.0.0/packages/coding-agent/examples/extensions) cover the whole API.
3. Import only what the runtime provides:
   - `@earendil-works/pi-coding-agent`, `pi-ai`, `pi-agent-core`, `pi-tui` and `typebox` resolve to copies bundled with yapi, as do the older `@mariozechner/*` names. Do not add them to `dependencies`.
   - Other npm packages go in the package's `dependencies`. yapi installs them without lifecycle scripts, so native addons stay unbuilt.
   - Node's built-in modules are shims. `fs`, `path`, `os`, `child_process`, `events`, `stream`, `util`, `crypto` hashes, `buffer`, timers and `fetch` work. Sockets, `http` requests, servers, `worker_threads`, `node:sqlite`, `zlib` compression and `vm` import but throw when used. Use `fetch` for HTTP.
   - Pi's internal classes, such as `SettingsManager` or `ExtensionRunner`, are not available. Stay within `ExtensionAPI`.
4. Check that it loads, without a model or the network:
   ```sh
   python3 scripts/check-extension.py extensions/<name>.ts
   ```
   It prints the commands the extension registered and every load error, and exits with 1 on an error. Then try it for real with `yapi -e extensions/<name>.ts` and call its command or ask the model to use its tool.
5. Consult [references/differences.md](references/differences.md) when something works in Pi but not in yapi.

## Native extension

Native WebAssembly extensions are unstable until yapi 1.0. The WIT world, the Rust SDK and the host requests they use may change in any release before then, and extensions may need to be rebuilt. Pi extensions from npm use Pi's extension API and are not affected.

1. Install the target once: `rustup target add wasm32-wasip2`.
2. Run `yapi new <name>`. It creates a Cargo project laid out as a package: a `cdylib` crate that depends on `yapi-extension-api` at the yapi repository's `v0.1.0` tag and registers a flag, a tool, a command and two event handlers, a `package.json` naming `extensions/<crate>.wasm`, and `.cargo/config.toml` making `wasm32-wasip2` the default target. `cargo generate --git https://github.com/SkymanOne/yapi --tag v0.1.0 crates/yapi/templates/extension` makes the same project.
3. Register everything in the init function and export it with `yapi_extension_api::extension!(init)`:

   | Item | Purpose |
   |---|---|
   | `Api::register_tool(Tool::new(name, description, schema, handler))` | A tool. `.label()`, `.prompt_snippet()` and `.prompt_guideline()` refine it. The handler is an `async` closure, `\|params, ctx\| async move { ... }`, that returns `ToolResult::text(...)`, optionally `.with_details(json)`. |
   | `Api::register_command(name, description, handler)` | `/name args`, with an `async` handler |
   | `Api::register_flag(name, FlagType, default, description)` | `--name`, read with `get_flag` |
   | `Api::register_shortcut(key, description, handler)` | A key such as `alt+k`, with an `async` handler that gets the `Context` |
   | `Api::on(event, handler)` | A Pi event, with an `async` handler. Return `Ok(None)`, or `Ok(Some(json))` with the handler's result in Pi's shape, such as `{"block": true, "reason": "..."}`. |
   | `notify`, `send_message`, `append_entry`, `exec`, `request` | Host actions that answer at once: notifications, session messages and entries, processes, and any other action by name |
   | `op`, `sleep`, `Process`, `spawn` | Async work: host operations such as dialogs and `fetch`, timers, processes read as they run, and background tasks that outlive the handler. A process inherits yapi's environment, and `"envAdd": {...}` in `exec.sync` or `Process::spawn` adds variables to it, where `env` would replace it. |
   | `events::on(channel, handler)`, `events::emit(channel, &data)` | Pi's `pi.events`, shared with every other extension, native or Pi. `emit` runs this extension's handlers at once and the others' after it returns. `on` returns a `Subscription` to `unsubscribe()`. |
   | `extension_path()` | The extension's `.wasm` file and the root of the package it was installed from, such as to run `yapi -e` with the extension itself |
   | `Context` | `mode()`, `has_ui()`, `cwd()`, `update()` for a tool's progress, and the rest of Pi's `ctx` as `data()` |
   | `Component`, `Context::custom`, `Context::set_widget`, `set_footer`, `set_header`, `set_editor_component` | Interface components, as Pi's `ctx.ui` shows them. A component's `render(width)` returns ANSI lines, `handle_input` gets raw keys (name them with `parse_key`), and `custom` resolves when it calls `Done::finish`. `theme()` styles text, `request_render()` redraws after changes not caused by input, and `terminal_size()` gives the terminal's columns and rows. Only the interactive mode shows components. |
   | `Context::on_terminal_input`, `Context::add_autocomplete_provider`, `editor_text` | Raw keys before the editor sees them, and completions in the editor, as Pi's `ctx.ui.onTerminalInput` and `ctx.ui.addAutocompleteProvider`. A listener returns `TerminalInput::Pass`, `Consume` or `Replace(data)` and stops when its `Subscription` drops. An `AutocompleteProvider` suggests and applies items with byte columns, asking the providers it wraps through `Current`. `editor_text()` reads the editor. |
   | `Tool::render_call`, `Tool::render_result`, `Api::register_message_renderer` | Components that draw tool calls, results and custom messages in the transcript |
   | `widgets` (feature `widgets`) | pi-tui's widgets as yapi ports them: `tui::select_list::SelectList`, `tui::editor::Editor`, `CustomEditor` for editor components, `Text`, and `to_ansi` to turn their styled lines into a component's lines |

4. Build, copy the build into the package, then check it loads:
   ```sh
   cargo build --release
   cp target/wasm32-wasip2/release/<crate>.wasm extensions/
   python3 scripts/check-extension.py .
   ```
5. The repository's [native examples](https://skymanone.github.io/yapi/native-examples.html) show a command guard, protected paths, a todo list kept per session branch and shown in a component, a footer component, a git status reporter, a subagent that runs another yapi in the foreground or the background, events shared with other extensions, scripts shipped in the package, a modal editor, a question tool with its own rendering, an overlay, a message renderer, a select list, and a terminal input listener with an autocomplete provider and a shortcut.

## Package and share

A package is a folder or npm package. Without a manifest, yapi loads every extension in its `extensions` folder. A manifest names the files, and yapi reads its `yapi` key before the `pi` key, so one package can serve Pi with JavaScript and yapi with a native build:

```json
{
  "name": "my-extension",
  "version": "0.1.0",
  "keywords": ["pi-package"],
  "pi": { "extensions": ["./extensions/my-extension.ts"] },
  "yapi": { "extensions": ["./extensions/my-extension.wasm"] }
}
```

- Install for testing: `yapi install ./my-extension` (add `-l` for the project only). `yapi list` tags it `[npm]`, `[wasm]` or both.
- Share it from git (`yapi install git:github.com/you/my-extension`) or npm (`yapi install npm:my-extension`).
- Commit or publish the built `.wasm` file. yapi never compiles code during installation.

## Before you finish

- `scripts/check-extension.py` reports no errors.
- The extension does what was asked when you run it with `yapi -e`.
- A Pi extension uses only `ExtensionAPI`, so it also runs in Pi.
- Tool descriptions and parameter descriptions tell the model when and how to call the tool.
