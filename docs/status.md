# Status

Progress against the milestones in [AGENTS.md](../AGENTS.md). Each milestone is delivered as a vertical slice: its exit criterion is met on the subset stated here, and what remains is listed. Intentional differences from pi are in [compat.md](compat.md); everything below is either done or not yet done.

Differential results come from the scenario suite (`cargo xtask e2e`), which compares ri with pi `v1.0.0` on the same cassettes.

## M0: workspace, types, mock server

Done.

- Workspace, CI on Linux and macOS, `cargo deny`.
- `ri-types` reads and writes every pi golden file byte-identically.
- `ri-mock` replays cassettes; `cargo xtask mock-sse` serves one to any client, or records one through a proxy with credentials redacted (`--record`).

## M1: wire APIs, print and JSON modes

Done for the slice below.

| Area | State |
|---|---|
| Wire APIs | `anthropic-messages`, `openai-completions` (all thinking formats and compat flags), `openai-responses`, `google-generative-ai` |
| Catalog | pi-ai 1.0.0 catalog embedded by `cargo xtask models`; `models.json` overrides and custom providers |
| Credentials | `--api-key`, `auth.json` (`!command`, `$VAR`), `models.json` `apiKey`, environment, ambient AWS and Vertex credentials |
| Modes | Print (`-p`) and JSON (`--mode json`); piped stdin; `@file` arguments |
| CLI | pi's argument parser, `--list-models`, `--help` |

Differential scenarios (all match pi): text, thinking, tool calls with result turns, HTTP errors, mid-stream errors, incomplete and max-token stops, piped stdin with a thinking suffix, model listing.

Not yet done:

- Live cassettes. The OpenCode Go key cannot reach `opencode.ai` from this environment; all cassettes are hand-written from the providers' documented stream formats.
- OpenAI grammar-constrained custom tools; such tools are sent as function tools.
- Image resizing and BMP conversion before upload.
- GitHub Copilot dynamic headers, Anthropic workload identity federation.
- Abort scenarios (cancellation is implemented and unit-tested, but has no differential scenario).

## M2: agent loop, tools, sessions

Done for the slice below.

- Agent loop with parallel tool execution, steering and follow-up queues, `AgentHooks`.
- All seven built-in tools. `grep` and `find` run `rg` and `fd` as pi does, downloading them when missing unless offline.
- System prompt sections, context files, skills, prompt templates, settings with project trust.
- Session manager: JSONL v3 tree, migration, branching, fork, clone, context edits, projection.
- Session selection: `--continue`, `--session` by path or id prefix, `--fork`, `--session-id`, custom session directories.
- Post-run recovery as in pi: auto-retry with backoff, omission of failed and truncated attempts, overflow recovery, threshold compaction with split-turn summaries, manual compaction.
- Tree navigation with branch summaries and labels.

Differential scenarios compare stdout, stderr, requests and the session files each program writes. Covered: new, continued, opened, forked and id-addressed sessions; retry exhausted and recovered; length stop; threshold compaction; parallel `ls`, `grep` and `find`; `@file` arguments.

Interop: the scenarios show ri writing the same session files as pi, line by line, and continuing sessions written in pi's format.

Not yet done:

- Steering, follow-up and abort have no differential scenario yet; they need RPC mode (M4).
- Tree navigation is tested with the faux provider only, until `/tree` exists (M3) or RPC (M4).

## M3: interactive TUI

Done for the slice below.

- Fullscreen and regular renderers, raw input decoding, Kitty keyboard negotiation, terminal color queries and the system theme.
- Transcript components: header, resources, messages with markdown and thinking, tool boxes with previews and diffs, `!` command output, summaries, status, warning and error lines, queued messages, editor and footer.
- Editor with autocomplete for commands, arguments, paths and `@` files.
- Selectors: model, thinking, fork, session (`/resume`, `--resume`), tree with filters, folding, labels and branch summaries, and the choice and text dialogs they use.
- Commands: `/model`, `/thinking`, `/export` (JSONL), `/import`, `/copy`, `/name`, `/session`, `/changelog`, `/hotkeys`, `/fork`, `/clone`, `/tree`, `/new`, `/compact`, `/reload`, `/debug`, `/resume`, `/quit`.
- Keys: interrupt and double escape, clear and exit, suspend, thinking and model cycling, model selector, tool and thinking toggles, external editor, copy, follow-up and dequeue, fullscreen scrolling.
- `!` and `!!` commands with streamed output, cancellation and session records.

Exit criterion. Golden suites recorded from pi-tui cover keys, input splitting, the editor, themes, text layout, markdown and autocomplete. 23 terminal scenarios, recorded from pi and run by `cargo test`, match pi's screens: startup, command autocomplete, a tool-call turn, `/session`, `!` and `!!`, `/tree` with its summary dialog and navigation, `/fork`, `/clone`, `/new`, `/name`, `/hotkeys`, the model and thinking selectors, `/model <ref>`, `/thinking <level>`, `@` and Tab completion, double escape and regular mode. `--resume` was compared by hand.

Budgets, from `cargo xtask bench` on this machine (release build, 100×40 terminal, a session of about 10,000 transcript lines for keystrokes):

| Metric | Budget | ri | pi |
|---|---|---|---|
| First paint | < 40 ms | 10 ms | 399 ms |
| Keystroke to paint, p50 | | 1.4 ms | 3.9 ms |
| Keystroke to paint, p99 | < 16 ms | 2.2 ms | 8.7 ms |

Not yet done:

- `/settings`, `/scoped-models`, `/trust`.
- HTML export; changelog entries.
- Clipboard image paste, terminal images, mermaid, mouse selection.
- Regular mode re-renders the whole document each frame, as pi does; fullscreen reuses unchanged rows.

## M4: RPC, MCP, OAuth, remaining wire APIs

Done for the slices below. The live OAuth check is pending.

RPC mode is done for the slice below.

- pi's JSONL protocol: LF framing, every command of `rpc-types.ts`, responses emitted as pi does (the prompt's at preflight, with its disposition), events in the JSON mode shapes, concurrent command handling, session replacement through the startup factory, and exit on end of input, SIGTERM (143) or SIGHUP (129).
- Shared with interactive mode: pi's runtime rules for new, forked, cloned and switched sessions, including forks of sessions that are not saved; pi's model and thinking level switching, which records a level only when it changes; and the queue display, which drops a queued message when it starts.

Exit criterion. Nine scenarios recorded from pi match: prompting and queries, steering, follow-up, clearing the queue, abort, session commands (entries, tree, fork messages, fork, switch, clone, new, naming), user bash commands, and settings, model and thinking commands. pi's `RpcClient` example (`tests/fixtures/pi/generator/rpc-client.mjs`) drives ri to the same output and requests as pi.

Not yet done:

- `export_html` (as `/export` to HTML in M3).
- `get_commands` lists prompt templates and skills; pi also lists its built-in `llama` and `mcp` extension commands.
- `cycle_model` over scoped models (`--models`, `enabledModels`).

MCP is done for the slice below.

- Client: a port of pi-mcp over stdio and streamable HTTP, with timeouts that progress restarts, cancellation, pagination, resumable event streams and pi's shutdown sequence for server processes.
- `mcp.json` from the agent directory and trusted projects, validated with pi's messages.
- The built-in extension: tools as `mcp__<server>__<tool>` with `direct`, `deferred` and `hidden` exposure and `toolExposure` overrides, the resource tools, the `mcp_servers` system prompt section, the startup wait for direct servers, problem reports, server logs in `mcp.log`, and `/mcp` status, `reconnect`, `login` and `logout` routing.
- The extension runner it needs: the `Extension` trait, a tool registry with pi's activation rules, `tool_search` with pi's BM25 ranking, extension commands from prompts, and notifications in every mode.

Exit criterion. MCP scenarios recorded from pi match: direct tools with text, structured, error, progress, image and resource-link results and the resource tools; deferred tools found and loaded by `tool_search`; and `/mcp` in RPC mode with configuration errors, a failed and a disabled server, reconnect and the subcommand errors. Client tests cover both transports against the test server.

Not yet done:

- `codemode` exposure, pi's default, needs the codemode tool (M7). Until then such servers warn that their tools are unreachable unless `tool_search` is active.
- OAuth sign-in for HTTP servers and `auth.provider` tokens.
- The `/mcp` manager in the TUI; `/mcp` shows the status instead.
- Servers registered by extensions (`pi.registerMcpServer`), and saving enable and exposure changes, which only the manager makes.
- pi's built-in `llama` extension.

OAuth is done for the slice below.

- Sign-in flows ported from pi-ai: Anthropic (Claude Pro/Max; browser callback or copied code), OpenAI Codex (browser callback or device code), Sign in with ChatGPT for `openai` (per-sign-in client registration, the installation's `deviceId`), and GitHub Copilot (device flow, enterprise domains, the account's endpoint from `proxy-ep`, model policies and the account's model list).
- `auth.json` as pi's credential store: reads follow the file's revision, changes take a `proper-lockfile`-compatible lock and rewrite the document as pi does. Tokens expiring within five minutes refresh under the lock after a second check, so concurrent ri and pi processes refresh once.
- `/login` and `/logout` in the TUI: pi's method menu, provider selector with configuration status, login dialog, API key login, default model selection after a first login, and the Anthropic subscription notice.
- GitHub Copilot's per-request headers on all three of its wire APIs.

Exit criterion. Each flow passes against a mock authorization server (`crates/ri-ai/tests/oauth.rs`): authorization URLs, pasted and loopback codes, device polling with `slow_down`, token exchange and refresh bodies, and refresh failures surfacing as request errors. Four `/login` and `/logout` screens recorded from pi match.

Not yet done:

- Sign-in for Kimi, Meta, xAI, OpenRouter and Radius. Their stored tokens are used as they are, without refresh, so a token copied from pi works until it expires.
- API key login for Bedrock, Vertex and Cloudflare, whose pi logins ask for more than a key; ri shows pi's "configured outside" notice.
- RPC mode has no login commands, as in pi.

The remaining wire APIs are done for the slice below.

- `openai-codex-responses`: ChatGPT's Codex backend with the account header, the instructions field, Codex's fixed request fields, its event mapping and usage-limit messages, over pi's SSE transport.
- `azure-openai-responses`: Responses on an Azure resource, with the base URL, API version and deployment map from pi's environment variables.
- `mistral-conversations`: Mistral's native chat endpoint, with pi's tool-call ids, reasoning effort or prompt mode, prompt caching and stream parser.

Exit criterion. Scenarios recorded from pi match for each: Codex text, tool calls, a usage limit and a failed response; Azure text and tool calls; Mistral text with reasoning effort, thinking in prompt mode, tool calls and an HTTP error.

Not yet done:

- Codex's WebSocket transport and zstd request compression. pi falls back to the same SSE requests when WebSockets fail.
- Amazon Bedrock, Google Vertex, Cloudflare and pi's `pi-messages` (Radius) wire APIs.

## M5: extension host, headless

Done for the slice below.

- `ri-js`, a WebAssembly component with QuickJS-NG, Node shims (`fs`, `path`, `os`, `child_process`, `events`, `util`, `crypto` hashes, `buffer`, timers, `fetch`, `Intl`), pi's extension API with pi's loading errors and per-extension event semantics, and the vendored `pi-tui` and `typebox`. `cargo xtask js-runtime` builds it; CI checks the committed artifact against its inputs.
- `ri-ext`: one actor thread per instance, a compiled-component cache, epoch-based compute limits, a memory limit, restart and replay after a trap, and grants for files, processes, network and environment.
- The host-side module loader: Node resolution with oxc, TypeScript stripping, ES module and CommonJS interop, jiti's `require`, `__dirname` and `__filename` in ES modules, and a transpile cache.
- Extension discovery from `-e`, a trusted project's `.ri/extensions` and the agent directory's `extensions`, with `package.json` manifests; pi's load errors, hint and exit code; extension flags on the command line, validated as pi does.
- Sessions: extension tools (with prompt snippets, guidelines, updates and activation rules), commands, and the events `session_start`, `session_shutdown`, `input`, `before_agent_start` (messages and a forced system prompt), `context`, `tool_call` (blocking), `tool_result` (changes), and the agent, turn, message and tool execution events. Actions: `sendMessage` with every delivery mode, `sendUserMessage`, `appendEntry`, session names and labels, the session manager's reads, active tools, thinking level, model selection, models and credentials, `exec`, notifications and the select, confirm and input dialogs. Each new session runs the factories again, as pi does.
- Extension errors are reported per mode: stderr in print and JSON modes, `extension_error` lines in RPC mode, error lines in the TUI.
- Packages: `ri install`, `remove` (`uninstall`), `update` and `list` with pi's arguments, messages and settings entries; npm, git and local sources; the user and project scopes; resources from `package.json` manifests (`ri` before `pi`, with globs and exclusions), conventional directories and settings filters; missing packages installed at startup unless offline; top-level `extensions` in settings; pi's extension order. npm packages install through a built-in client: registry resolution with npm's range syntax, integrity checks, hoisted dependencies, pruning on removal, no lifecycle scripts (native addons stay unbuilt and fail only when loaded) and pi's own packages skipped. `cargo xtask vendor-pi` regenerates the vendored bundles reproducibly.
- Native extensions: `guest/ri-extension-api`, a Rust SDK for tools, commands, flags, event handlers and synchronous host actions, with an example (`guest/examples/hello`). A `.wasm` file loads wherever a pi extension file does, in its own instance.

Exit criterion. All 79 of pi's example extensions register the same tools (with schemas), commands, flags, shortcuts and event handlers as in pi, and fail with pi's messages where pi does (`crates/ri-ext/tests/examples.rs`). The criterion asks for 90%.

Of the 50 most-downloaded npm pi packages (`tests/fixtures/pi/packages/top50.json`), 45 install with ri's npm client and register the same as in pi (`cargo xtask package-registrations`, which runs extensions without network, process or environment access). Two that pi itself fails to load under its harness sandbox (pi-lens, @raindrop-ai/pi-agent) are left out, which makes 45 of 48, or 94%. The other three:

- context-mode loads `better-sqlite3`, a native addon, when it starts.
- gentle-pi uses `createCodemodeExtension`, which needs the codemode tool (M7).
- pi-fabric patches pi's internal `ExtensionRunner`.

Nine scenarios recorded from pi match: an extension tool called by the model with a result handler, a prompt message and a flag (JSON mode); a command that sends a message (RPC mode); load failures; an unknown flag; a package directory whose manifest names its extension; and the package commands (local install, list, empty list, removing an unknown package). The npm client is tested against a mock registry.

Not yet done:

- `message_end` replacements, `tool_call` handlers that change the call's input, and the `before_provider_request`, `user_bash`, `model_select`, `resources_discover` and `session_before_*` events.
- Command context actions that replace the session (`newSession`, `fork`, `navigateTree`, `switchSession`, `reload`), `ctx.executeTool`, and completions through pi-ai from extensions.
- `session_start` always reports the reason `startup`; `pi.sendUserMessage` expands prompt templates.
- Themes from packages; package update checks at startup; registry credentials from `.npmrc`; temporary installs for `-e npm:` and `-e git:` sources.
- Asynchronous host operations (timers, processes, HTTP) in the Rust SDK.

## M6: extension UI

Done for the slice below.

- `ExtensionUi` covers pi's `ExtensionUIContext`: dialogs with timeouts, the editor dialog, footer statuses, widgets above and below the editor, a replaced footer and header, the terminal title, the working message, the hidden thinking label, the editor's text, pasting, tool expansion and the theme.
- Remote components: pi-tui components stay in the extension runtime by handle. The TUI paints the lines of their last render and asks for a new render when they are stale, so a frame never waits for JS. Keys go to the focused component; `tui.requestRender()` marks components stale.
- `ctx.ui.custom` in the editor's place or as an overlay (pi-tui's layout and compositing), component widgets, `setFooter` and `setHeader` factories, tool `renderCall` and `renderResult` (with `lastComponent`, shared state and `renderShell: "self"`) and message renderers. Custom messages with `display: true` show in the transcript.
- The theme reaches extensions as escape sequences per token, so `theme.fg`, `theme.bg` and the facade's list, editor, settings and markdown themes produce pi's output.
- The startup listing names loaded extensions as pi does, compactly or by scope.
- RPC mode sends `extension_ui_request` lines for statuses, widgets (lines only), the title, the editor text and the editor dialog, and passes dialog timeouts.

Exit criterion. 24 TUI scenarios recorded from pi match row for row at 80×24 and 120×40: widgets and footer statuses during a turn, pi's timed confirm and select examples, select, confirm, input and editor dialogs open and answered, a custom component open and answered, custom messages with and without pi's message renderer example, the editor text, pi's `question.ts` tool driven by the model, with its custom component and its call and result renderers, and pi's `overlay-test.ts` overlay with wide characters, emoji and inline input. An RPC scenario matches pi's `extension_ui_request` lines, themed status text included.

Not yet done:

- Overlays: `nonCapturing` and `visible` options, and `OverlayHandle` focus and visibility changes.
- `onTerminalInput`, `setEditorComponent`, `addAutocompleteProvider`, `setWorkingVisible`, `setWorkingIndicator` and `setTheme`; dialog `signal` options.
- Entry renderers (`registerEntryRenderer`); extension keybindings inside components use pi-tui's defaults, not `keybindings.json`.
- A theme change reaches extensions when their session next starts.
- The `/mcp` manager.

## M7: codemode, budgets, import, release

In progress.

- `ri import pi` copies pi's agent directory (settings, credentials, models, keybindings, MCP servers, trust, system prompt files, sessions, prompts, skills, themes, extensions and packages) and the current project's `.pi` into ri's, keeping files ri already has.
- A tag-triggered release workflow builds stripped binaries for Linux and macOS on x86_64 and arm64, with checksums.

Budgets, from `cargo xtask bench --pi` on this machine (release build, 100×40 terminal; keystrokes in a session of about 10,000 transcript lines; memory sampled 2 s after first paint; the 10 extensions each register a tool, a command and an event handler):

| Metric | Budget | ri | pi |
|---|---|---|---|
| `--version` | < 5 ms | 2.0 ms | 268 ms |
| Print mode, start to first request byte | < 25 ms | 14.5 ms | 372 ms |
| Interactive first paint | < 40 ms | 9.7 ms | 372 ms |
| Keystroke to paint, p99 | < 16 ms | 2.4 ms | 8.3 ms |
| Idle memory, no extensions | < 30 MB | 15.8 MiB | 107 MiB |
| Idle memory, 10 JS extensions | < 70 MB | 28.6 MiB | 112 MiB |
| Stripped release binary | < 35 MB | 33.2 MB | |

Not yet done:

- Codemode.
- The budgets measured on macOS.

## Pending live checks

These need the user's credentials and run outside CI:

- OpenCode Go recording (Anthropic, Completions and Responses routes), once `opencode.ai` is reachable.
- OAuth sign-in with real subscription accounts: Anthropic, ChatGPT (Codex and `openai`), GitHub Copilot.
