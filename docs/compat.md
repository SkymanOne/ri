# Differences from Pi

yapi follows Pi `v1.0.0` and reads Pi's settings, sessions, credentials and packages. It behaves like Pi except as listed here. Any other difference is a bug: please [report it](https://github.com/SkymanOne/yapi/issues).

## Extensions

Pi extensions run unchanged, in WebAssembly rather than Node.js.

| Area | Pi | yapi |
|---|---|---|
| Runtime | Pi's Node.js process | QuickJS-NG in WebAssembly: 60 seconds of computing per call, 1 GiB per instance, restart after a crash. See the [security model](configuration.md#security-model). |
| Node APIs | All of Node | [Shims](extensions.md#how-pi-extensions-run). Sockets, servers, `http` requests, `worker_threads`, `node:sqlite`, `zlib`, `vm` and `child_process.fork` throw when used. `require()` of an ES module fails. |
| Native addons | Loaded | Installed, never built. Loading one fails. |
| Pi internals | Every export of Pi's packages | The extension API and its helpers. Other exports throw when called, and built-in tool factories reject custom `operations`. |
| Agent sessions in an extension | `createAgentSession` and Pi's SDK | Not supported, by design: such a session would run tools and use credentials outside the extension's grants. Run a subagent as another yapi, with Pi's `RpcClient` or as Pi's `subagent` example does. |
| Extension UI | Components detect links, images and theme changes | Links and images fall back to text. Theme changes reach extensions at their next session. |
| Terminal input listeners | Pi handles each key before the next one reaches them | They run while yapi keeps drawing, so keys that arrive together pass through them together. |
| Autocomplete providers | The editor waits for providers | yapi's editor never waits: `applyCompletion` runs for every suggestion, and `signal` never aborts. Editors from `setEditorComponent` behave as in Pi. |
| Failing session actions | A throwing `setup` or `withSession` callback ends interactive mode | yapi reports the error and keeps the current session. |
| Console output | stdout and stderr | stderr only, because stdout carries print, JSON and RPC output. |
| Project trust | Extensions loaded before trust can answer `project_trust` | No extension loads before you trust the project. |
| Codemode scripts | May compute without limit | Stop after 60 seconds of computing without awaiting a call. |
| Native extensions | None | WebAssembly components written in Rust. See [Native extensions](native-extensions.md). Unstable until 1.0. |

## Packages and installation

| Area | Pi | yapi |
|---|---|---|
| Directories | `~/.pi/agent`, project `.pi/`, `PI_CODING_AGENT_DIR` | `~/.yapi/agent`, project `.yapi/`, `YAPI_CODING_AGENT_DIR`, and `YAPI_` in place of `PI_` for the session directory variable. Same formats, and `yapi import pi` copies Pi's state. |
| Package installs | `npm install`, with lifecycle scripts | A built-in npm client that skips lifecycle scripts. `npmCommand` applies to npm sources only. |
| npm configuration | npm reads all its configuration files and settings | Registries and credentials from `~/.npmrc`, or the file `npm_config_userconfig` names, and from `npm_config_*` variables. Other settings, such as `cafile`, are ignored. |
| Package manifests | The `pi` key | A `yapi` key of the same shape takes precedence, so one package can ship a native build for yapi. |
| Self-update | `pi update` | `yapi update` updates packages and models only. See [Install](install.md#upgrade). |
| `new` | `pi new ...` is a prompt | `yapi new` creates a native extension. Quote a prompt that starts with `new`. |

## Interface and providers

| Area | Pi | yapi |
|---|---|---|
| `/share` and `/bug` | Use Pi's services | Not available. |
| Clipboard paste | Copied files, images and text on every platform | Nothing on Windows, and images copied in Windows do not reach WSL. |
| Word motions in Chinese, Japanese and Thai | By dictionary word | By character, to keep ICU's dictionaries out of the binary. |
| Tree label times | Local time | UTC, with no time zone database. |
| Codex transport | WebSocket, then compressed SSE | Uncompressed SSE, the same requests and events as Pi's fallback. |
| Image formats | Any format Pi's image library reads becomes PNG | Only BMP is converted. Other prompt images are left out with a note. |
| Mouse in fullscreen mode | Clicks also choose items in `/settings`, `/thinking` and their submenus, move the cursor in selector search fields, and expand tool output, thinking and summaries. Extension editors and components get mouse events. | Clicks move the editor's cursor and choose completions. Other built-in components get no mouse events. Extension headers, message renderers and tool renderers get none either. Other extension components get presses, clicks and the wheel, but no pointer moves, drags or releases, and a click does not give them the keyboard focus. |

## Open Gaps

Pi features yapi does not have yet, or has only in part. Each row links its issue, and the row goes when the gap closes.

| Area | Pi | yapi today | Issue |
|---|---|---|---|
| Entry renderers and markdown transformers | `registerEntryRenderer` and `registerMarkdownTransformer` change how entries and markdown render | Recorded but unused. | [#30](https://github.com/SkymanOne/yapi/issues/30) |
| Markdown | LaTeX as Unicode math, highlighted code blocks, Mermaid diagrams | LaTeX as written, code blocks and Mermaid source in the code block color. | [#31](https://github.com/SkymanOne/yapi/issues/31) |
| Syntax highlighting in tool rows | `read` results, `write` previews and codemode scripts are highlighted | Shown in the default color. | [#32](https://github.com/SkymanOne/yapi/issues/32) |
| Cache warming | Refreshes the prompt cache while the model streams (`cacheWarming`) and reports cache misses | Sends no refresh requests. The setting is saved and ignored. | [#33](https://github.com/SkymanOne/yapi/issues/33) |
| Images in the terminal | Inline images, with the "Show images" and "Image width" settings | Not shown. | [#34](https://github.com/SkymanOne/yapi/issues/34) |
| Update notices | Checks at startup for new versions and package updates and shows a notice | Makes no such checks and shows no update notices. `/changelog` and the list of changes after an upgrade work as in Pi. | [#37](https://github.com/SkymanOne/yapi/issues/37) |
| Codemode | Scripts use grammar-constrained sampling on the Responses APIs, and a script waits for the MCP servers it names | Scripts are ordinary tool calls, and scripts do not wait for servers. | [#38](https://github.com/SkymanOne/yapi/issues/38) |
| OpenAI custom tools | Sent as grammar-constrained tools | Sent as function tools. | [#39](https://github.com/SkymanOne/yapi/issues/39) |
| Full extension providers | `registerProvider` also takes a complete pi-ai `Provider` object, image and classifier models with their implementations, and the legacy `oauth.modifyModels` | A provider configuration only. Those parts are ignored. | [#54](https://github.com/SkymanOne/yapi/issues/54) |
| `newSession` setup | `setup` gets the new session's writable session manager before its extensions start | `setup` runs after the new session's `session_start` and gets the read-only session manager. | [#67](https://github.com/SkymanOne/yapi/issues/67) |
