# Where Pi extensions behave differently in yapi

yapi runs Pi extensions in a QuickJS-NG runtime compiled to WebAssembly, not in Node.js. Most extensions behave as in Pi. These are the differences an extension author meets. The full list is [Differences from Pi](https://skymanone.github.io/yapi/compat.html).

## Runtime

- Each call into the extension may compute for 60 seconds, and the runtime may use 1 GiB of memory. A trapped runtime restarts and its extensions load again.
- Node's built-in modules are shims. These import but throw when called: `http` requests, `net` sockets and servers, `tls`, `worker_threads` workers, `node:sqlite`, `zlib` compression, `vm`, `v8`, `dns`, `http2`, `dgram`, `cluster`, `inspector`, `child_process.fork`, `util.parseArgs`, `stream.pipeline`, `stream.finished`, `events.on` and `process.chdir`. `fetch` and `child_process` work.
- `require()` of an ES module fails. Use `import`.
- Native addons are installed unbuilt and fail when loaded. Load them lazily, so the rest of the extension still works.
- `Intl` covers Unicode segmentation and number and plural formatting in `en-US`. `Intl.DateTimeFormat` ignores its options.
- `process.arch` is `wasm32`. Console output goes to stderr.
- Error stacks show only frames in the extension's own files.

## API

- Pi's internal classes, such as `SettingsManager`, `ModelRuntime` and `ExtensionRunner`, import but throw when called. Built-in tool factories run yapi's tools and reject custom `operations`.
- `ctx.ui.setTheme` takes theme names only, not `Theme` objects.
- `newSession`'s `setup` runs after the new session's `session_start` with the read-only session manager, so it cannot append entries.
- `onTerminalInput`, `setEditorComponent` and `addAutocompleteProvider` have no effect. `registerEntryRenderer`, `registerMarkdownTransformer` and `registerMcpServer` are recorded but unused.
- `registerProvider` works for providers configured like `models.json` entries, without a custom `streamSimple`, an OAuth sign-in or `refreshModels`.

## Interface

- Components render inside the runtime, and yapi paints their last frame, so a slow component never blocks the interface.
- pi-tui reports a terminal without hyperlinks or images, so links show as text and images use pi-tui's text fallback.
- Components match keys against pi-tui's default bindings, not the user's `keybindings.json`.
- A theme change reaches extensions when their session next starts.

## Packages

- yapi reads a `yapi` key in `package.json` before the `pi` key.
- Dependencies install with yapi's own npm client, without lifecycle scripts or `.npmrc` credentials.
