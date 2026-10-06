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
| Extension UI | Components detect hyperlink and image support, see theme changes at once and match keys against your `keybindings.json` | Links and images in components fall back to pi-tui's text forms, theme changes reach extensions when their session next starts, and components match pi-tui's default key bindings. |
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
| Extension events | Every event of the extension API | Never sends `before_provider_request`, `user_bash`, `model_select`, `resources_discover`, `session_before_*` or `agent_before_settle`. `session_start` always has the reason `startup`, and `turn_end` handlers cannot stage entries or continue the run. | [#21](https://github.com/SkymanOne/yapi/issues/21) |
| Extension session actions | `newSession`, `fork`, `navigateTree`, `switchSession`, `reload` and `setTheme` work | The session actions fail and `setTheme` reports failure. | [#22](https://github.com/SkymanOne/yapi/issues/22) |
| Custom editors and input hooks | `setEditorComponent`, `onTerminalInput` and `addAutocompleteProvider` work | They have no effect. | [#23](https://github.com/SkymanOne/yapi/issues/23) |
| Extension providers | `registerProvider` accepts a custom `streamSimple`, an OAuth sign-in and `refreshModels` | Only providers configured like `models.json` entries. A model whose API only an extension implements cannot be the session's model. | [#24](https://github.com/SkymanOne/yapi/issues/24) |
| Documentation for the model | Installs its docs and examples. The system prompt points the model at them, and the header says Pi can explain its own features. | Ships no docs, so the model cannot look up how yapi or its extension API works. | [#25](https://github.com/SkymanOne/yapi/issues/25) |
| Images sent to models | Resized to fit 2000×2000 (`images.autoResize`). BMP converted. | Sent unresized. A BMP image or one over the size limit is replaced by Pi's omission note. | [#26](https://github.com/SkymanOne/yapi/issues/26) |
| MCP sign-in | OAuth for HTTP servers and `auth.provider` tokens | `yapi mcp login` and `/mcp login` report that sign-in is not available. | [#27](https://github.com/SkymanOne/yapi/issues/27) |
| MCP manager | `/mcp` opens a manager, and `pi.registerMcpServer` adds servers | `/mcp` shows the status only, and `pi.registerMcpServer` has no effect. | [#28](https://github.com/SkymanOne/yapi/issues/28) |
| Temporary packages | `-e npm:<name>` and `-e git:<url>` install a package for one run | Not supported. | [#29](https://github.com/SkymanOne/yapi/issues/29) |
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
