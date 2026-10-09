# Where Pi extensions behave differently in yapi

yapi runs Pi extensions in a QuickJS-NG runtime compiled to WebAssembly, not in Node.js. Most extensions behave as in Pi. These are the differences an extension author meets. The full list is [Differences from Pi](https://skymanone.github.io/yapi/compat.html).

## Runtime

- Each call into the extension may compute for 60 seconds, and the runtime may use 1 GiB of memory. A trapped runtime restarts and its extensions load again.
- Node's built-in modules are shims. These import but throw when called: `http` requests, `net` sockets and servers, `tls`, `worker_threads` workers, `node:sqlite`, `zlib` compression, `vm`, `v8`, `dns`, `http2`, `dgram`, `cluster`, `inspector`, `child_process.fork`, `util.parseArgs`, `stream.pipeline`, `stream.finished`, `events.on` and `process.chdir`. `fetch` and `child_process` work.
- `require()` of an ES module fails. Use `import`.
- Native addons are installed unbuilt and fail when loaded. Load them lazily, so the rest of the extension still works.
- `Intl` covers Unicode segmentation and number and plural formatting in `en-US`. `Intl.DateTimeFormat` ignores its options.
- `process.arch` is `wasm32`. Console output goes to stderr in print, JSON and RPC modes. Interactive mode keeps it off the screen, and `/debug` writes its last 200 lines to `~/.yapi/agent/yapi-debug.log`.
- Error stacks show only frames in the extension's own files.

## API

- Pi's internal classes, such as `SettingsManager`, `ModelRuntime` and `ExtensionRunner`, import but throw when called. Built-in tool factories run yapi's tools and reject custom `operations`.
- `createAgentSession` and the rest of Pi's SDK throw, by design: yapi does not run agent sessions inside an extension's sandbox. Run a subagent as another yapi instead. Pi's `RpcClient` starts yapi in RPC mode and can prompt, steer, follow up and abort it. Pi's `subagent` example runs `child_process.spawn(process.execPath, ["--mode", "json", "-p", "--no-session", task])` and reads its events as they arrive.
- `newSession`'s `setup` runs after the new session's `session_start` with the read-only session manager, so it cannot append entries.
- `pi.events` listeners in the emitter's runtime instance run during `emit`, as in Pi. Native extensions and other instances hear the event right after `emit` returns, with a JSON copy of the data.
- `onTerminalInput` handlers run while yapi keeps drawing, so keys that arrive together pass through them before yapi handles the first.
- In yapi's own editor, an autocomplete provider's `applyCompletion` runs for every suggestion as they arrive, and its `signal` never aborts. An editor from `setEditorComponent` behaves as in Pi.
- `registerEntryRenderer` and `registerMarkdownTransformer` are recorded but unused.
- `registerMcpServer` needs the extension's `process` grant for a stdio server and its `network` grant for an HTTP server. With the default grants it behaves as in Pi.
- `registerProvider` does not take a complete pi-ai `Provider` object, image or classifier models, or `oauth.modifyModels`.

## Interface

- Components render inside the runtime, and yapi paints their last frame, so a slow component never blocks the interface.
- pi-tui reports a terminal without hyperlinks or images, so links and images use pi-tui's text fallbacks. A hyperlink a component writes itself still works.
- In fullscreen mode, components get presses, clicks and the wheel through `handleMouse`, but no pointer moves, drags or releases, and a click does not give them the keyboard focus. Headers, message renderers and tool renderers get no mouse events.
- A theme change reaches extensions when their session next starts.

## Packages

- yapi reads a `yapi` key in `package.json` before the `pi` key.
- Dependencies install with yapi's own npm client, without lifecycle scripts. It reads registries and credentials from `~/.npmrc` and `npm_config_*` variables, not from a project's `.npmrc` or npm's global configuration.
