# Differences from Pi

yapi follows Pi `v1.0.0`. It reads Pi's settings, sessions, credentials and packages, and behaves like Pi unless this page says otherwise. Any other difference is a bug, and the [issue tracker](https://github.com/SkymanOne/yapi/issues) is the place to report it.

This page lists only differences that change what you can do. Small differences in wording, error messages or product names are not listed.

## Extensions

Pi extensions run unchanged, but not in Pi's Node.js process. yapi runs them in WebAssembly, which limits what they can reach.

| Area | Pi | yapi |
|---|---|---|
| Where extensions run | In Pi's Node.js process, with your permissions | In a QuickJS-NG runtime compiled to WebAssembly. Each call may compute for 60 seconds and each instance may use 1 GiB of memory. An instance that traps restarts and its extensions load again. Grants are full by default, see the [security model](configuration.md#security-model). |
| Node APIs | All of Node | The shims listed in [Extensions](extensions.md#how-pi-extensions-run). These import but throw when used: `http` requests, `net` sockets and servers, `tls`, `worker_threads` workers, `node:sqlite`, `zlib` compression, `vm`, `v8`, `dns` queries other than `lookup`, `http2`, `dgram`, `cluster`, `inspector`, `child_process.fork`, `util.parseArgs`, `stream.pipeline`, `stream.finished`, `events.on` and `process.chdir`. `require()` of an ES module fails. |
| Native addons | Loaded by Node | Installed but not built. Loading one fails. The rest of the package still works if it loads the addon lazily. |
| Pi internals | Extensions can import all of `pi-coding-agent`, `pi-ai` and `pi-agent-core` | The extension API and the helpers extensions use. Other exports import but throw when called, and built-in tool factories reject custom `operations`. |
| `Intl` | ICU in every locale | Number and plural formatting in `en-US` only, and `Intl.DateTimeFormat` ignores its options. |
| Extension UI | Components detect hyperlink and image support and see theme changes at once | Links and images in components fall back to pi-tui's text forms, and theme changes reach extensions when their session next starts. |
| Terminal input listeners | `onTerminalInput` handlers run on each key, and Pi handles the key before the next one reaches them | Handlers run in the extension's runtime while yapi keeps drawing, so the interface never waits on an extension. Keys that arrive together, or while handlers run, pass through the handlers together before yapi handles the first of them. |
| Autocomplete providers | The editor calls a provider's `applyCompletion` for the suggestion picked and its `shouldTriggerFileCompletion` on Tab. A newer request aborts the `signal` of an older one. | yapi's editor never waits for a provider, so `applyCompletion` runs for every suggestion as the suggestions arrive, Tab follows the built-in `shouldTriggerFileCompletion`, and the `signal` never aborts. An editor from `setEditorComponent` behaves as in Pi. |
| Failing session actions | When the `setup` or `withSession` callback of `newSession`, `fork` or `switchSession` throws, interactive mode prints "Failed to fork session" or "Failed to create session" and exits | yapi reports the extension's error and keeps the current session open. |
| Console output | Written to stdout and stderr | Written to stderr, because stdout carries print, JSON and RPC output. |
| Project trust | Extensions loaded before trust can answer the `project_trust` event | yapi asks before loading any extension, so no extension runs in a project you have not trusted. |
| Synchronous process timeouts | `execSync` and `spawnSync` send SIGTERM and wait for the process to exit | SIGTERM, then SIGKILL after 5 seconds, as Pi's `exec` does. |
| Codemode scripts | Runaway recursion throws a catchable `RangeError`. A script without `timeout_ms` may compute without limit. | Runaway recursion or 60 seconds of computing without awaiting a call stops the script with a sandbox failure. |
| Native extensions | Not available | yapi also loads WebAssembly components written with its Rust SDK. See [Native extensions in Rust](native-extensions.md). They are unstable until yapi 1.0. |

## Packages and installation

| Area | Pi | yapi |
|---|---|---|
| Directories | `~/.pi/agent`, project `.pi/`, `PI_CODING_AGENT_DIR`, `PI_CODING_AGENT_SESSION_DIR` | `~/.yapi/agent`, project `.yapi/`, `YAPI_CODING_AGENT_DIR`, `YAPI_CODING_AGENT_SESSION_DIR`. The formats are the same, and `yapi import pi` copies Pi's state. |
| Package installs | `npm install`, which runs lifecycle scripts. `npmCommand` replaces npm for every package operation. | A built-in npm client that skips lifecycle scripts and needs no Node.js. `npmCommand` is used only to install and update npm sources. |
| Package manifests | The `pi` key in `package.json` | The `pi` key, or a `yapi` key of the same shape that takes precedence, so one package can ship JavaScript for Pi and a native build for yapi. |
| Self-update | `pi update` updates Pi | `yapi update` updates packages and model catalogs only. Upgrade yapi itself as [Install](install.md#upgrade) describes. |
| `yapi new` | `pi new ...` sends the words as a prompt | Creates a native extension project. A prompt that starts with the word `new` needs quotes. |

## Interface and providers

| Area | Pi | yapi |
|---|---|---|
| `/share` and `/bug` | Publish the session to pi.dev's viewer and send reports to Pi's developers | Not available, because they are Pi's services. |
| Clipboard | A native clipboard addon, then platform commands and OSC 52 | Platform commands and OSC 52. |
| Word motions in Chinese, Japanese and Thai | Move by dictionary words | Move one character at a time, because ICU's word dictionaries would add megabytes to the binary. |
| Tree label times | Local time | UTC, because yapi carries no time zone database. |
| Codex transport | WebSocket first, then SSE with a compressed body | SSE with an uncompressed body. Requests and events are the same as Pi's fallback. |
| Image formats | Images from RPC clients, extensions and tools in any format Pi's image library reads, such as TIFF, are converted to PNG for the model | BMP is the only format converted. Prompt images in other formats are left out with a note, and tool results pass them on unchanged. |

## Open Gaps

Pi features yapi does not have yet, or has only in part. Each row links its issue, and the row goes when the gap closes.

| Area | Pi | yapi today | Issue |
|---|---|---|---|
| MCP manager | `/mcp` opens a manager, and `pi.registerMcpServer` adds servers | `/mcp` shows the status only, and `pi.registerMcpServer` has no effect. | [#28](https://github.com/SkymanOne/yapi/issues/28) |
| Entry renderers and markdown transformers | `registerEntryRenderer` and `registerMarkdownTransformer` change how entries and markdown render | Recorded but unused. | [#30](https://github.com/SkymanOne/yapi/issues/30) |
| Markdown | LaTeX as Unicode math, bare URLs as links, highlighted code blocks, Mermaid diagrams | LaTeX as written, bare URLs as plain text, code blocks and Mermaid source in the code block color. | [#31](https://github.com/SkymanOne/yapi/issues/31) |
| Syntax highlighting in tool rows | `read` results, `write` previews and codemode scripts are highlighted | Shown in the default color. | [#32](https://github.com/SkymanOne/yapi/issues/32) |
| Cache warming | Refreshes the prompt cache while the model streams (`cacheWarming`) and reports cache misses | Sends no refresh requests. The setting is saved and ignored. | [#33](https://github.com/SkymanOne/yapi/issues/33) |
| Images in the terminal | Inline images, with the "Show images" and "Image width" settings | Not shown. | [#34](https://github.com/SkymanOne/yapi/issues/34) |
| Clipboard image paste | `app.clipboard.pasteImage` attaches the clipboard's image | The key does nothing. | [#35](https://github.com/SkymanOne/yapi/issues/35) |
| Mouse selection | Selects text in fullscreen mode, and `fullscreenCopyOnSelect` | No mouse selection. The setting is saved and ignored. | [#36](https://github.com/SkymanOne/yapi/issues/36) |
| `/changelog` and update notices | Shows the changelog and notices of new versions and package updates | `/changelog` reports no entries, and no notices are shown. | [#37](https://github.com/SkymanOne/yapi/issues/37) |
| Codemode | `codemode.mode: "only"` hides direct tools, scripts use grammar-constrained sampling on the Responses APIs, and a script waits for the MCP servers it names | `only` acts as `on`, scripts are ordinary tool calls, and scripts do not wait for servers. | [#38](https://github.com/SkymanOne/yapi/issues/38) |
| OpenAI custom tools | Sent as grammar-constrained tools | Sent as function tools. | [#39](https://github.com/SkymanOne/yapi/issues/39) |
| Private npm registries | npm reads `.npmrc`, including registry credentials | `npm_config_registry` or the public registry, without credentials. | [#40](https://github.com/SkymanOne/yapi/issues/40) |
| Streamed `fetch` responses | `fetch` streams the response body | `fetch` resolves once the whole response has arrived, so a provider extension that streams over `fetch` shows its events when the response ends. | [#53](https://github.com/SkymanOne/yapi/issues/53) |
| Full extension providers | `registerProvider` also takes a complete pi-ai `Provider` object, image and classifier models with their implementations, and the legacy `oauth.modifyModels` | A provider configuration only. Those parts are ignored. | [#54](https://github.com/SkymanOne/yapi/issues/54) |
| Extension providers in `--list-models` | `--list-models` lists the models extensions register | Lists built-in and `models.json` models only. | [#55](https://github.com/SkymanOne/yapi/issues/55) |
| `newSession` setup | `setup` gets the new session's writable session manager before its extensions start | `setup` runs after the new session's `session_start` and gets the read-only session manager. | [#67](https://github.com/SkymanOne/yapi/issues/67) |
| Theme objects | `ctx.ui.setTheme` takes a theme name or a `Theme` object | A theme name only. A `Theme` object returns an error. | [#66](https://github.com/SkymanOne/yapi/issues/66) |
| Compact reads | Reads of its docs, skills and resource files show in a compact form | Every `read` shows a full tool box. | [#50](https://github.com/SkymanOne/yapi/issues/50) |
| Extension tool event order | A tool that publishes updates and calls `ctx.executeTool()` reports its events in order | The updates and the nested call's events can come out in a different order. | [#59](https://github.com/SkymanOne/yapi/issues/59) |
| Bedrock payloads in `before_provider_request` | Extensions see the Bedrock command input, with `modelId` | Extensions see the HTTP body, without `modelId`. | [#62](https://github.com/SkymanOne/yapi/issues/62) |
