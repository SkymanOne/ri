# Differences from Pi

yapi follows Pi `v1.0.0`. This page lists every known difference: first the intentional ones, then the Pi features yapi has not ported yet. Anything not listed here is expected to match Pi, and a mismatch is a bug.

## Files and installation

| Area | Pi | yapi | Reason |
|---|---|---|---|
| Directories | `~/.pi/agent`, project `.pi/`, `PI_CODING_AGENT_DIR`, `PI_CODING_AGENT_SESSION_DIR` | `~/.yapi/agent`, project `.yapi/`, `YAPI_CODING_AGENT_DIR`, `YAPI_CODING_AGENT_SESSION_DIR`. `yapi import pi` copies Pi state. | Same formats without two tools writing one directory. |
| Package manifest | `pi` key | `pi` key, plus an optional `yapi` key with the same shape that takes precedence | Lets one package ship a native wasm build for yapi and JS for Pi. |
| Package installation | Runs `npm install`, which runs lifecycle scripts. `npmCommand` replaces npm for installs, removals and git dependencies, with arguments adapted for bun and pnpm. | A built-in npm client that skips lifecycle scripts. `npmCommand` installs and updates npm sources with npm's arguments. Removals and git dependencies always use the built-in client. | No Node dependency. Lifecycle scripts mostly build native addons, which yapi cannot load. |
| npm registry | npm's configuration, including `.npmrc` credentials and its global install root | `npm_config_registry` or the public registry, without credentials | The built-in client reads only what it needs to install public packages. |
| `yapi list` | Each package's source and install location | The same, with a tag after each installed package that has extensions: `[npm]`, `[wasm]` or `[npm, wasm]` | Shows which packages run as Pi extensions and which as native ones. |
| `yapi new` | `pi new ...` sends the words as a prompt | Creates a Cargo project for a native extension package. A prompt that starts with the word `new` needs quotes, as in `yapi "new tests for the parser"`. | Native extensions are yapi's own. |
| `yapi update` | Updates Pi itself (default), packages, or model catalogs | Updates packages and model catalogs. Asking to update yapi reports that yapi cannot update itself and names the install script. | yapi ships as a release binary. |
| Unpaired UTF-16 surrogates in JSON strings | Read and written as `\udXXX` escapes | A line containing one fails to parse | A Rust `String` cannot hold them. Only malformed text, such as truncated model output, produces them. |
| Agent markers for child processes | `PI_CODING_AGENT=true`, `AI_AGENT=pi` | `PI_CODING_AGENT=true`, `AI_AGENT=yapi` | `AI_AGENT` names the running agent. `PI_CODING_AGENT` keeps scripts that check for Pi working. |
| Debug log, external editor | `pi-debug.log`, "Pi will resume when the editor exits." | `yapi-debug.log`, "yapi will resume when the editor exits." | Product name. |
| MCP client identity, saved outputs | `clientInfo` `pi` and Pi's version. `pi-mcp-*`, `pi-codemode-*` and `pi-bash-*` temp files. | `yapi` and yapi's version. `yapi-mcp-*`, `yapi-codemode-*` and `yapi-bash-*` temp files. | Product name. |
| Sign in with ChatGPT | `agent_name_hint` `Pi` | `yapi` | Product name shown on OpenAI's consent page. |

## Extensions

| Area | Pi | yapi | Reason |
|---|---|---|---|
| Where extensions run | In Pi's Node.js process, with the user's permissions | In WebAssembly sandboxes: Pi extensions in a QuickJS-NG runtime, native ones as components built with yapi's Rust SDK. Each call may compute for 60 seconds and each instance may use 1 GiB of memory. A trapped instance restarts and its extensions load again. | Memory isolation, resource limits and grants that the host enforces. See [Extensions](extensions.md). |
| Native extensions | Extensions are TypeScript or JavaScript | Also WebAssembly components, loaded from `.wasm` files. Pi extensions run first, then native ones. | The sandboxable extension tier. See [Native extensions in Rust](native-extensions.md). |
| Node APIs | All of Node | The shims listed in [Extensions](extensions.md#how-pi-extensions-run). These import but throw when used: `http` requests, `net` sockets and servers, `tls`, `worker_threads` workers, `node:sqlite`, `zlib` compression, `vm`, `v8`, `dns`, `http2`, `dgram`, `cluster`, `inspector`, `child_process.fork`, `util.parseArgs`, `stream.pipeline`, `stream.finished`, `events.on` and `process.chdir`. `net` address checks and `BlockList` work. `require()` of an ES module fails. `process.arch` is `wasm32`. | The wasm runtime has no sockets, threads or SQLite, and QuickJS loads ES modules asynchronously. |
| Native addons | Loaded by Node | Installed unbuilt. Loading one fails, and the rest of the package still works when it loads its addon lazily. | The wasm runtime cannot load native code. |
| `Intl` | ICU in every locale | Unicode segmentation, plus number and plural formatting in `en-US`. `Intl.DateTimeFormat` ignores its options and formats as `Date.prototype.toLocaleString()` does, in UTC unless `TZ` is set. | QuickJS has no ICU, and yapi carries no locale data. |
| Pi packages imported by extensions | All of `pi-coding-agent`, `pi-ai` and `pi-agent-core` | The extension API (gaps are listed under [Not yet ported](#not-yet-ported)) and the helpers extensions use. Other exports import but throw when called. Built-in tool factories run yapi's tools and reject custom `operations`. A stream registered with `registerApiProvider` serves the extension's own pi-ai calls, but a model whose API only an extension implements cannot be the session's model. | Pi's internals have no counterpart inside the runtime. |
| Component output | pi-tui detects the terminal's hyperlink (OSC 8) and image support | pi-tui inside yapi always reports neither, so links show as text and images use pi-tui's text fallback. Escapes emitted directly by extensions are stripped. | yapi's terminal cells cannot carry OSC 8 or image escapes. |
| Theme updates | Components see a theme change at once | Extensions see the new theme when their session next starts | The theme crosses into the runtime as escape sequences per token. |
| Keybindings | Components match keys against the user's `keybindings.json` | Components match pi-tui's default bindings | The keybindings manager inside the runtime has no user configuration. |
| Error stacks | Every frame of Node's stack, Pi's own frames included | Only the frames in the extension's files, as QuickJS reports them | yapi's runtime frames say nothing about the extension, and QuickJS formats frames differently from V8. |
| Console output | Written to Pi's stdout and stderr | Written to stderr | Stdout belongs to print, JSON and RPC output. |
| Project trust | Extensions loaded before trust can answer a `project_trust` event before Pi asks | yapi asks before loading any extension | Extensions never run in a project the user has not trusted. |

## Interface

| Area | Pi | yapi | Reason |
|---|---|---|---|
| Startup header | Pi logo, version, key hints and "Pi can explain its own features and look up its docs" | A two-row "y" logo of the same size (the `yapi` wordmark in Apple Terminal), version and key hints | yapi is not Pi and ships no documentation for the model. The same-size logo keeps the hints wrapping as Pi's do. |
| System prompt | Preamble says "operating inside pi". A `docs` section points to Pi's installed documentation. | Preamble says "operating inside yapi", with no `docs` section. Where a model's output limit is capped by its context window, yapi's `max_tokens` is about 350 tokens higher. | yapi installs no documentation. |
| `--help`, no-models message | Mention `PI_PACKAGE_DIR` and Pi's installed docs. `--help` lists `PI_TELEMETRY` and `PI_SHARE_VIEWER_URL`. | Omit them | yapi has no package or docs directory, sends no install telemetry, and has no `/share`. |
| Sign-in help in errors | Points to `providers.md` and `models.md` in Pi's install | Points to the "Models and sign-in" section of yapi's README | yapi installs no documentation files. |
| `/share`, `/bug`, `/arminsayshi`, `/dementedelves` | `/share` publishes the session as a secret GitHub gist for pi.dev's viewer. `/bug` sends a report to Pi's developers or exports it. The others are easter eggs. Provider errors and crash notices suggest `/bug`. | Each reports that it is not available in yapi. Errors do not mention `/bug`. | They belong to Pi's services and brand. |
| Clipboard | Native clipboard addon first, then platform commands and OSC 52. Fullscreen flashes "Copied!". | Platform commands and OSC 52, with a status line confirming the copy | yapi loads no native addons. |
| `/llama` | A model manager component with load and download progress bars. Escape asks to cancel a load or download. | The same choices through standard select, input and confirm dialogs, with progress on the status line. A load or download runs until the server finishes it. | Pi's manager is a custom TUI component. yapi's built-ins use the dialogs every mode shares. |
| Cancelling a sign-in dialog | Escape shows `Failed to save API key for <provider>: This operation was aborted` (or `Failed to login to <provider>: …`) and returns to the editor | Escape reopens the selector or menu the sign-in started from | Pi's `onBack` handler intends this, but the abort error preempts it. |
| Escape during a tool run | The next request fails with `stopReason: "error"` and "This operation was aborted", because its signal is not the one aborted | The run ends as aborted, with `stopReason: "aborted"` | The user aborted the run. Recording a provider error misreports it. |
| `--resume` without a terminal | Opens the session picker anyway | Exits with an error naming `--session` and `--continue` | The picker needs a terminal to draw in. |
| Terminal input outside the Basic Multilingual Plane | Delivered as two lone UTF-16 surrogates, one per input event | Delivered as one character | Rust strings hold whole characters. The inserted text is the same. |
| Word motions in Chinese, Japanese and Thai | `Intl.Segmenter`'s dictionaries find words, so Ctrl+W, Alt+B/F and Ctrl+Left/Right move by word | Unicode word boundaries (UAX #29), which move one character at a time in these scripts | ICU's word dictionaries would add megabytes to a binary close to its size budget. |
| Editor wrapping of a character wider than the editor | Recurses until the stack overflows | Gives the character a line of its own | Pi crashes at a one-column width. |
| Tree label times | Local time | UTC | yapi carries no time zone database. |
| Invalid `httpIdleTimeoutMs` | Startup fails with `Invalid httpIdleTimeoutMs setting` | The default of five minutes applies | A mistyped setting should not stop every mode from starting. |

## Protocols and messages

| Area | Pi | yapi | Reason |
|---|---|---|---|
| JSON mode `message_update.usage`, assistant `message_start` | Serialized from the live message, so they show state from later in the stream | State at the moment of the event | Pi's values depend on stream timing. |
| Invalid RPC command arguments | Accepted as JavaScript coerces them: an unknown thinking level becomes `off`, an unknown steering mode is stored, `bash` without `command` runs `undefined` | Rejected with an error response that names the invalid value or missing field | The values have no meaning, and acting on them hides the client's bug. |
| Model objects in RPC responses | Keys in the order of each model's catalog entry, which varies between models | One fixed key order | The values are the same. yapi's catalog is generated into one type. |
| Codex transport | WebSocket first (`transport` setting), then SSE with a zstd-compressed body | SSE with an uncompressed body | Same requests and events as Pi's fallback, without a WebSocket client or zstd encoder in the binary. |
| `models.json` errors | JSON.parse and TypeBox messages under Pi's headings, such as `Invalid models.json schema:` with one line per path | The same headings and file line, with serde's message for the first problem | The messages come from different parsers. |
| Parse errors in other JSON files: RPC commands, themes, `settings.json`, `mcp.json`, codemode `@options` | Pi's prefix, such as `Failed to parse command:`, followed by V8's `JSON.parse` message | The same prefix followed by serde_json's message | Parser messages are implementation details. |
| Codemode reference | The `models` line of the tool description points to `docs/codemode.md` in Pi's install | It points to an adapted copy that yapi writes to `~/.yapi/agent/docs/codemode.md` when codemode is active | yapi installs no documentation files. |
| Codemode recursion | Runaway recursion throws a catchable `RangeError` | It ends the script with `Script sandbox failed: wasm trap: …` | QuickJS-NG does not bound its stack on WASI, so the wasm stack limit stops the instance instead. |
| Codemode compute | A script without `timeout_ms` may compute without limit | A script that computes for 60 seconds without awaiting a call stops with a sandbox failure | Every extension instance has this limit. |

## Not yet ported

These Pi features are missing from yapi or work only in part. [dev/status.md](https://github.com/SkymanOne/yapi/blob/main/dev/status.md) tracks them per milestone.

| Area | Pi | yapi |
|---|---|---|
| Markdown | marked: LaTeX shown as Unicode math, GFM bare URLs become links, code blocks syntax-highlighted. Mermaid code blocks drawn as diagrams. | pulldown-cmark: LaTeX shown as written, bare URLs stay plain text, code blocks and Mermaid source in the code block color. Text and layout otherwise match. |
| Syntax highlighting in tool rows | `read` results, `write` previews and codemode scripts are highlighted | Shown in the default color |
| Cache warming | Sends cache-refresh requests while the model streams (`cacheWarming`, default `streaming`), records their usage, can show cache-miss notices, and `/session` reports both | Sends none. `/session` reports cache warming as unavailable and omits cache-miss costs. The setting is saved and ignored. |
| Images sent to models | Resized to fit 2000×2000 (`images.autoResize`). BMP converted. | Sent unresized. A BMP image or one over the size limit is replaced by Pi's omission note. |
| Images in the terminal | Inline images, with the "Show images" and "Image width" settings | Not shown |
| Clipboard image paste | `app.clipboard.pasteImage` attaches the clipboard's image | The key does nothing |
| Mouse selection | Selecting text in fullscreen mode, and `fullscreenCopyOnSelect` | No mouse selection. The setting is saved and ignored. |
| `/changelog`, update notices | Pi's changelog, and notices of new Pi versions and package updates at startup | `/changelog` reports no entries, and no notices are shown |
| MCP | OAuth sign-in for HTTP servers, the `/mcp` manager, and servers registered by extensions | `yapi mcp login` and `/mcp login` report that sign-in is not available, `/mcp` shows the status, and `pi.registerMcpServer` has no effect |
| Codemode | `codemode.mode: "only"` hides direct tools. Scripts use grammar-constrained sampling on the Responses wire APIs. | `only` acts as `on`. Scripts are sent as ordinary tool calls. |
| Extension events | Every event of the extension API | Never sent: `before_provider_request`, `user_bash`, `model_select`, `resources_discover`, `session_before_*` and `agent_before_settle`. `session_start` always has the reason `startup`. `turn_end` handlers cannot stage entries or continue the run. |
| Extension actions | Every action of the extension API | `newSession`, `fork`, `navigateTree`, `switchSession` and `reload` fail. `onTerminalInput`, `setEditorComponent` and `addAutocompleteProvider` have no effect, and `setTheme` reports failure. `registerEntryRenderer` and `registerMarkdownTransformer` are recorded but unused. `registerProvider` supports providers configured like `models.json` entries, without a custom `streamSimple`, an OAuth sign-in or `refreshModels`. |
| Temporary packages | `-e npm:<name>` and `-e git:<url>` install a package for one run | Not supported |
