# Deviations from pi

Intentional differences from pi `v1.0.0`. Anything not listed here is expected to match pi; a mismatch is a bug.

| Area | pi | ri | Reason |
|---|---|---|---|
| Directories | `~/.pi/agent`, project `.pi/`, `PI_CODING_AGENT_DIR` | `~/.ri/agent`, project `.ri/`, `RI_CODING_AGENT_DIR`; `ri import pi` copies pi state | Same formats without two tools writing one directory. |
| Package manifest | `pi` key | `pi` key, plus an optional `ri` key with the same shape that takes precedence | Lets one package ship a native wasm build for ri and JS for pi. |
| Package installation | Runs `npm install` (or `npmCommand`), which runs lifecycle scripts | Built-in npm client that skips lifecycle scripts; `npmCommand` still overrides it | No Node dependency. Lifecycle scripts mostly build native addons, which ri cannot load. |
| Unsupported packages | Any Node code runs | Packages with native addons, `net`/`tls` servers or `worker_threads` are rejected | Not available inside the wasm runtime. |
| Unpaired UTF-16 surrogates in JSON strings | Read and written as `\udXXX` escapes | A line containing one fails to parse | A Rust `String` cannot hold them. Only malformed text, such as truncated model output, produces them. |
| Extension component output | Uses the terminal's hyperlink (OSC 8) and image support | pi-tui inside ri reports neither, so links print their URL as text and images use pi-tui's text fallback. Escapes emitted directly by extensions are stripped. | ratatui cells cannot carry OSC 8 or image escapes. |
| System prompt | Preamble says "operating inside pi"; a `docs` section points to pi's installed documentation | Preamble says "operating inside ri"; no `docs` section, so where a model's output limit is capped by its context window, ri's `max_tokens` is about 350 tokens higher | ri installs no documentation. |
| `--help`, no-models message | Mention `PI_PACKAGE_DIR` and pi's installed docs | Omit both | ri has no package or docs directory. |
| JSON mode `message_update.usage`, assistant `message_start` | Serialized from the live message, so they show state from later in the stream | State at the moment of the event | pi's values depend on stream timing. |
| Terminal input outside the Basic Multilingual Plane | Delivered as two lone UTF-16 surrogates, one per input event | Delivered as one character | Rust strings hold whole characters; the inserted text is the same. |
| Markdown | marked: LaTeX shown as Unicode math, GFM bare URLs become links, code blocks syntax-highlighted | pulldown-cmark: LaTeX shown as written, bare URLs stay plain text, code blocks in the code block color | Not yet ported; text and layout otherwise match pi's renderer. |
| Startup header | pi logo, version, key hints and "Pi can explain its own features and look up its docs" | `ri` wordmark, version and key hints | ri is not pi and ships no documentation for the model to read. |
| Model catalogs | `/model` and the model selector refresh provider catalogs over the network | The catalog is built in; the selector reports it refreshed and `/model <ref>` searches it directly | ri has no runtime catalog sources yet. |
| Cache warming | `/session` reports the cache warming state and cache-miss costs | `/session` reports cache warming as unavailable and omits cache-miss costs | ri does not keep provider caches warm. |
| Completion timing | Suggestions resolve after the current input chunk, so an Enter in the same chunk as the text submits it | Suggestions are computed at once, so that Enter applies the highlighted completion first | Synchronous completion keeps the editor single-threaded; typed input behaves the same. |
| Clipboard | Native clipboard addon first, then platform commands and OSC 52; fullscreen flashes "Copied!" | Platform commands and OSC 52; a status line confirms the copy | ri loads no native addons. |
| Tree label times | Local time | UTC | ri carries no time zone database. |
| `/share`, `/bug`, `/arminsayshi`, `/dementedelves` | Upload to pi's services; easter eggs | Report that the command is not available | They belong to pi's services and brand. |
| Debug log, external editor | `pi-debug.log`; "Pi will resume when the editor exits." | `ri-debug.log`; "ri will resume when the editor exits." | Product name. |
| RPC parse errors | `Failed to parse command:` followed by V8's `JSON.parse` message | The same prefix followed by serde_json's message | Parser messages are implementation details. |
| MCP client identity, saved outputs | `clientInfo` `pi` and pi's version; `pi-mcp-*` temp files | `ri` and ri's version; `ri-mcp-*` temp files | Product name. |
| Sign in with ChatGPT | `agent_name_hint` `Pi` | `ri` | Product name shown on OpenAI's consent page. |
| `/login` providers | pi's built-in `llama.cpp` extension adds a provider; Radius offers a sign-in | Neither | Not yet ported; the Radius gateway also needs pi's `pi-messages` wire API. |
| Node APIs in extensions | All of Node | The shims listed in [status.md](status.md); modules ri cannot provide, such as `http`, `net` and `worker_threads`, import but throw when used. `require()` of an ES module fails. | The wasm runtime has no sockets or threads; QuickJS loads ES modules asynchronously. |
| `Intl` in extensions | ICU in every locale | Unicode segmentation; number, date and plural formatting in `en-US` only | QuickJS has no ICU, and ri carries no locale data. |
| pi APIs in extensions | All of `pi-coding-agent`, `pi-ai` and `pi-agent-core` | The extension API and the helpers extensions use; other exports import but throw when called. Built-in tool factories run ri's tools and reject custom `operations`. | pi's internals have no counterpart inside the runtime. |
| Extension console output | Written to pi's stdout and stderr | Written to stderr | Stdout belongs to print, JSON and RPC output. |
| Codex transport | WebSocket first (`transport` setting), then SSE with a zstd-compressed body | SSE with an uncompressed body | Same requests and events as pi's fallback, without a WebSocket client or zstd encoder in the binary. |
