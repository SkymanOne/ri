# Status

Progress against the milestones in [AGENTS.md](../AGENTS.md). Each milestone is delivered as a vertical slice: its exit criterion is met on the subset stated here, and what remains is listed. Intentional differences from pi are in [compat.md](../docs/compat.md); everything below is either done or not yet done.

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

Live cassettes recorded through OpenCode Go with pi as the client (`tests/fixtures/cassettes/opencode-go`) cover `anthropic-messages` (MiniMax M3), `openai-completions` (DeepSeek V4 Flash) and `openai-responses` (GPT-5.6 Luna). Each has a text answer with the model's thinking and a tool call with its result turn; the Responses streams carry encrypted reasoning. Their six scenarios match pi. The other cassettes are hand-written from the providers' documented stream formats.

Not yet done:

- OpenAI grammar-constrained custom tools; such tools are sent as function tools.
- Image resizing and BMP conversion before upload.
- Anthropic workload identity federation.

## M2: agent loop, tools, sessions

Done for the slice below.

- Agent loop with parallel tool execution, steering and follow-up queues, `AgentHooks`.
- All seven built-in tools. `grep` and `find` run `rg` and `fd` as pi does, downloading them when missing unless offline.
- System prompt sections, context files, skills, prompt templates, settings with project trust.
- Session manager: JSONL v3 tree, migration, branching, fork, clone, context edits, projection.
- Session selection: `--continue`, `--session` by path or id prefix, `--fork`, `--session-id`, custom session directories.
- Post-run recovery as in pi: auto-retry with backoff, omission of failed and truncated attempts, overflow recovery, threshold compaction with split-turn summaries, manual compaction. Summary requests retry under the same policy and report it with pi's `summarization_retry_*` events.
- Tree navigation with branch summaries and labels.

Differential scenarios compare stdout, stderr, requests and the session files each program writes. Covered: new, continued, opened, forked and id-addressed sessions; retry exhausted and recovered; length stop; threshold compaction; parallel `ls`, `grep` and `find`; `@file` arguments.

Steering, follow-up and abort are covered by the RPC scenarios (M4), and tree navigation by the `/tree` scenarios (M3).

Interop: the scenarios show ri writing the same session files as pi, line by line, and continuing sessions written in pi's format.

## M3: interactive TUI

Done for the slice below.

- Fullscreen and regular renderers, raw input decoding, Kitty keyboard negotiation, terminal color queries and the system theme.
- Transcript components: header, resources, messages with markdown and thinking, skill invocations, tool boxes with previews and diffs, `!` command output, summaries, status, warning and error lines, queued messages, editor and footer.
- Fullscreen layout as pi's flex stack: a dock taller than the screen leaves the transcript one row and shrinks its parts in proportion, each keeping its top rows or its cursor.
- The fullscreen scrollbar (`fullscreenScrollbar`: `auto` while scrolling, `always`, `hidden`), the jump-to-latest label over the last row, and the line, half-page and prompt-to-prompt scroll keys.
- Built-in `edit` calls preview their diff, or why they cannot apply, inside the tool box while they run.
- Editor with autocomplete for commands, arguments, paths and `@` files.
- Selectors: model (with the all/scoped toggle), thinking, fork, session (`/resume`, `--resume`), tree with filters, folding, labels and branch summaries, and the choice and text dialogs they use.
- `/settings` with pi's items, search and submenus (warnings, per-model thinking levels, the theme with live preview and automatic light/dark pairs). Changes apply at once where ri implements the setting, including switching between fullscreen and regular mode, and are saved as pi saves them.
- `/scoped-models`: enabling, clearing, reordering and provider toggles for the session, saved to `enabledModels` with `ctrl+s`.
- `httpIdleTimeoutMs`: provider requests fail when headers or body chunks stop arriving for that long, as pi's undici timeouts do, and `/settings` changes it at once.
- `terminal.showTerminalProgress`: pi's OSC 9;4 progress while the agent runs or compacts, repeated every second and cleared at the end and on exit.
- `images.blockImages` replaces images sent to providers with pi's notice; `fullscreenExitOutput: "resume-hint"` leaves fullscreen without printing the transcript.
- Commands: `/model`, `/thinking`, `/settings`, `/scoped-models`, `/export` (JSONL), `/import`, `/copy`, `/name`, `/session`, `/changelog`, `/hotkeys`, `/fork`, `/clone`, `/tree`, `/new`, `/compact`, `/reload`, `/debug`, `/resume`, `/quit`.
- Keys: interrupt and double escape, clear and exit, suspend, thinking and model cycling, model selector, tool and thinking toggles, external editor, copy, follow-up and dequeue, fullscreen scrolling.
- `!` and `!!` commands with streamed output, cancellation and session records.
- Themes from settings entries, packages, the agent's and a trusted project's `themes` directories and `--theme`, registered by the name each declares, with pi's `[Theme conflicts]` listing; `--use-theme` and `--no-themes`.
- Project trust: the startup prompt before anything project-local loads, with session-only choices, `/trust` and the untrusted-project notice.
- HTML export through `/export`, `--export` and RPC `export_html`: pi's viewer template, unchanged, with the session, prompt, tools and theme colors filled in as pi does; `--export` output is byte-identical to pi's.
- A settings file that fails to parse is reported as a warning and left untouched; ri runs without it, as pi does.
- A session whose recorded directory no longer exists opens only after pi's prompt to continue in the current directory; print, JSON and RPC modes refuse it with pi's message.
- Notices and command output (`/session`, `/hotkeys`, `/changelog`) re-wrap when the terminal is resized, as pi's text components do.
- On SIGTERM or SIGHUP every mode kills the commands it started, whose process groups would otherwise outlive it.
- Model scope from `--models` or `enabledModels`, with pi's glob and `:level` patterns: the startup model, the "Model scope" line, cycling, `/model` completion and the footer's provider count.
- `/reload` keeps the model and thinking level; a resumed session keeps its thinking level whatever chose the model; an aborted response reads "Operation aborted" or "Aborted after N retry attempts", as pi writes it.

Exit criterion. Golden suites recorded from pi-tui cover keys, input splitting, the editor, themes, text layout, markdown and autocomplete. 23 terminal scenarios, recorded from pi and run by `cargo test`, match pi's screens: startup, command autocomplete, a tool-call turn, `/session`, `!` and `!!`, `/tree` with its summary dialog and navigation, `/fork`, `/clone`, `/new`, `/name`, `/hotkeys`, the model and thinking selectors, `/model <ref>`, `/thinking <level>`, `@` and Tab completion, double escape and regular mode. `--resume` was compared by hand.

Budgets, from `cargo xtask bench` on this machine (release build, 100×40 terminal, a session of about 10,000 transcript lines for keystrokes):

| Metric | Budget | ri | pi |
|---|---|---|---|
| First paint | < 40 ms | 10 ms | 399 ms |
| Keystroke to paint, p50 | | 1.4 ms | 3.9 ms |
| Keystroke to paint, p99 | < 16 ms | 2.2 ms | 8.7 ms |

Not yet done:

- Settings that `/settings` saves but ri does not act on yet: image auto-resize, cache warming and cache-miss notices, mermaid diagrams, copy on select, the condensed changelog and install telemetry.
- Changelog entries.
- Clipboard image paste, terminal images, mermaid, mouse selection.
- Regular mode re-renders the whole document each frame, as pi does; fullscreen reuses unchanged rows.

## M4: RPC, MCP, OAuth, remaining wire APIs

Done for the slices below. The live OAuth check is pending.

RPC mode is done for the slice below.

- pi's JSONL protocol: LF framing, every command of `rpc-types.ts`, responses emitted as pi does (the prompt's at preflight, with its disposition), events in the JSON mode shapes, concurrent command handling, session replacement through the startup factory, and exit on end of input, SIGTERM (143) or SIGHUP (129).
- Shared with interactive mode: pi's runtime rules for new, forked, cloned and switched sessions, including forks of sessions that are not saved; pi's model and thinking level switching, which records a level only when it changes; and the queue display, which drops a queued message when it starts.

Exit criterion. Nine scenarios recorded from pi match: prompting and queries, steering, follow-up, clearing the queue, abort, session commands (entries, tree, fork messages, fork, switch, clone, new, naming), user bash commands, and settings, model and thinking commands. pi's `RpcClient` example (`tests/fixtures/pi/generator/rpc-client.mjs`) drives ri to the same output and requests as pi. `get_commands` lists the built-in `llama` and `mcp` commands before prompt templates and skills, as pi does.

MCP is done for the slice below.

- Client: a port of pi-mcp over stdio and streamable HTTP, with timeouts that progress restarts, cancellation, pagination, resumable event streams and pi's shutdown sequence for server processes.
- `mcp.json` from the agent directory and trusted projects, validated with pi's messages.
- `ri mcp add`, `remove` and `list` (with `--json`), which edit `mcp.json` with its own indentation and check servers outside a session, as `pi mcp` does.
- The built-in extension: tools as `mcp__<server>__<tool>` with `direct`, `deferred` and `hidden` exposure and `toolExposure` overrides, the resource tools, the `mcp_servers` system prompt section, the startup wait for direct servers, problem reports, server logs in `mcp.log`, and `/mcp` status, `reconnect`, `login` and `logout` routing.
- pi's tool renderers: the `server/tool` label, output colored by outcome and collapsed to five wrapped rows with the path of output saved for the model. A server that cannot be reached reports `fetch failed`, as Node's `fetch` does.
- The extension runner it needs: the `Extension` trait, a tool registry with pi's activation rules, `tool_search` with pi's BM25 ranking, extension commands from prompts, and notifications in every mode.

Exit criterion. MCP scenarios recorded from pi match: direct tools with text, structured, error, progress, image and resource-link results and the resource tools; deferred tools found and loaded by `tool_search`; and `/mcp` in RPC mode with configuration errors, a failed and a disabled server, reconnect and the subcommand errors. 16 `ri mcp` scenarios match pi's output and exit codes, and the `mcp.json` both write is byte-identical. Client tests cover both transports against the test server.

Not yet done:

- pi's wait, before a script runs, for the `codemode` servers it names that have not connected yet.
- OAuth sign-in for HTTP servers and `auth.provider` tokens, and so `ri mcp login` and `logout`, which report it.
- The `/mcp` manager in the TUI; `/mcp` shows the status instead.
- Servers registered by extensions (`pi.registerMcpServer`), and saving enable and exposure changes, which only the manager makes.

OAuth is done for the slice below.

- Sign-in flows ported from pi-ai: Anthropic (Claude Pro/Max; browser callback or copied code), OpenAI Codex (browser callback or device code), Sign in with ChatGPT for `openai` (per-sign-in client registration, the installation's `deviceId`), and GitHub Copilot (device flow, enterprise domains, the account's endpoint from `proxy-ep`, model policies and the account's model list).
- `auth.json` as pi's credential store: reads follow the file's revision, changes take a `proper-lockfile`-compatible lock and rewrite the document as pi does. Tokens expiring within five minutes refresh under the lock after a second check, so concurrent ri and pi processes refresh once.
- `/login` and `/logout` in the TUI: pi's method menu, provider selector with pi's configuration status labels and `models.json` provider names, `/login <provider>` argument completion, login dialog, API key login, default model selection after a first login, and the Anthropic subscription notice.
- GitHub Copilot's per-request headers on all three of its wire APIs.
- `ri auth print-api-key`, `print-bearer-token` and `check` for external clients, with pi's resolution, refresh, JSON output and exit codes.

Exit criterion. Each flow passes against a mock authorization server (`crates/ri-ai/tests/oauth.rs`): authorization URLs, pasted and loopback codes, device polling with `slow_down`, token exchange and refresh bodies, and refresh failures surfacing as request errors. Eight `/login` and `/logout` screens recorded from pi match, and 16 `ri auth` scenarios match pi's output and exit codes.

Also done: sign-in for Kimi Code, Meta, xAI, OpenRouter and the Radius gateway, with pi's refresh rules, and pi's `/login` questions for Amazon Bedrock, Google Vertex and Cloudflare. RPC mode has no login commands, as in pi.

The remaining wire APIs are done for the slice below.

- `openai-codex-responses`: ChatGPT's Codex backend with the account header, the instructions field, Codex's fixed request fields, its event mapping and usage-limit messages, over pi's SSE transport.
- `azure-openai-responses`: Responses on an Azure resource, with the base URL, API version and deployment map from pi's environment variables.
- `mistral-conversations`: Mistral's native chat endpoint, with pi's tool-call ids, reasoning effort or prompt mode, prompt caching and stream parser.

Exit criterion. Scenarios recorded from pi match for each: Codex text, tool calls, a usage limit and a failed response; Azure text and tool calls; Mistral text with reasoning effort, thinking in prompt mode, tool calls and an HTTP error.

Every provider in pi-ai 1.0 is also done:

- `bedrock-converse-stream`: AWS event streams, SigV4 (checked against a signature pi's SDK produced), bearer tokens, the SDK's retries and the default credential chain.
- `google-vertex`: API keys and Application Default Credentials, including service account impersonation and external accounts.
- Cloudflare Workers AI and AI Gateway, with account and gateway placeholders in the base URL.
- `pi-messages`, the wire API of pi's Radius gateway, whose catalog ri fetches after sign-in.
- Anthropic workload identity federation.
- The local llama.cpp router, from pi's built-in `llama.cpp` extension: `/login llama.cpp`, `LLAMA_BASE_URL`, the router's models in the catalog, and `/llama` to load, unload and download models.
- Classifier models (`typesafe-system-one`, `cloudflare-workers-ai-system-one`, `llama-cpp-classify`) and image models (`openrouter-images`), for codemode's `models` global and `ctx.modelRegistry` in extensions.
- Catalog refreshes as pi's `ModelRuntime` makes them: pi.dev's overlay for configured providers, Radius and llama.cpp catalogs, kept in `models-store.json`.

Bedrock, Vertex, Cloudflare and federation requests and errors are checked against mock servers, with bodies taken from requests pi made with fake credentials (`crates/ri-ai/tests/providers.rs`). Classifier and image requests and results are checked against pi-ai's (`tests/fixtures/pi/models-api`). Scenarios recorded from pi cover the Radius and llama.cpp sign-in screens, the model selector's refresh states and `get_commands`. No request reached a real Bedrock, Vertex, Cloudflare, Radius or llama.cpp endpoint, so each is listed under pending live checks.

Not yet done:

- Codex's WebSocket transport and zstd request compression. pi falls back to the same SSE requests when WebSockets fail.

## M5: extension host, headless

Done for the slice below.

- `ri-js`, a WebAssembly component with QuickJS-NG, Node shims (`fs`, `path`, `os`, `child_process`, `events`, `util`, `crypto` hashes, `buffer`, timers, `fetch`, `Intl`), pi's extension API with pi's loading errors and per-extension event semantics, and the vendored `pi-tui` and `typebox`. `cargo xtask js-runtime` builds it; CI checks the committed artifact against its inputs.
- `ri-ext`: one actor thread per instance, a compiled-component cache, epoch-based compute limits, a memory limit, restart and replay after a trap, and grants for files, processes, network and environment.
- The host-side module loader: Node resolution with oxc, TypeScript stripping, ES module and CommonJS interop, jiti's `require`, `__dirname` and `__filename` in ES modules, and a transpile cache.
- Extension discovery from `-e`, a trusted project's `.ri/extensions` and the agent directory's `extensions`, with `package.json` manifests; pi's load errors, hint and exit code; extension flags on the command line, validated as pi does. As in pi, `-e` on a directory loads it as a temporary package with its skills, prompts and themes.
- Sessions: extension tools (with prompt snippets, guidelines, updates and activation rules), commands, and the events `session_start`, `session_shutdown`, `input`, `before_agent_start` (messages and a forced system prompt), `context`, `tool_call` (blocking), `tool_result` (changes), and the agent, turn, message and tool execution events. Actions: `sendMessage` with every delivery mode, `sendUserMessage`, `appendEntry`, session names and labels, the session manager's reads, active tools, thinking level, model selection, models and credentials, `exec`, notifications and the select, confirm and input dialogs. Each new session runs the factories again, as pi does.
- Extension errors are reported per mode: stderr in print and JSON modes, `extension_error` lines in RPC mode, pi's error lines with the stack's extension frames in the TUI. A failing command is named `command:<name>`, as in pi.
- Packages: `ri install`, `remove` (`uninstall`), `update` and `list` with pi's arguments, messages and settings entries; npm, git and local sources; the user and project scopes; resources from `package.json` manifests (`ri` before `pi`, with globs and exclusions), conventional directories and settings filters; missing packages installed at startup unless offline; top-level `extensions` in settings; pi's extension order. npm packages install through a built-in client: registry resolution with npm's range syntax, integrity checks, hoisted dependencies, pruning on removal, no lifecycle scripts (native addons stay unbuilt and fail only when loaded) and pi's own packages skipped. `cargo xtask vendor-pi` regenerates the vendored bundles reproducibly.
- `ri config`: pi's resource selector, globally or for the project (`-l`), which writes the same settings patterns. Package commands and `ri config` ask whether to trust the project in a terminal, as pi does.
- Resources resolve as pi's package manager resolves them: auto-discovered directories (including `.agents/skills` up to the git root), settings entries, packages and built-ins, with `+path`, `-path` and `!glob` overrides, ignore files, project precedence and per-package `autoload` deltas. `crates/ri-core/tests/resolve.rs` compares the result with pi's on eight trees (`tests/fixtures/pi/resolve`).
- Native extensions: `guest/ri-extension-api`, a Rust SDK for tools, commands, flags, event handlers and synchronous host actions, with five examples in `guest/examples` (`hello`, and ports of pi's `permission-gate`, `protected-paths` and `todo`, plus `repo-status`), each tested in `crates/ri-ext/tests/native.rs`. A `.wasm` file loads wherever a pi extension file does, in its own instance. `ri list` tags packages `[npm]`, `[wasm]` or both.

Exit criterion. All 79 of pi's example extensions register the same tools (with schemas), commands, flags, shortcuts and event handlers as in pi, and fail with pi's messages where pi does (`crates/ri-ext/tests/examples.rs`). The criterion asks for 90%.

The 500 most-downloaded npm pi packages (`tests/fixtures/pi/packages/top500.json`, from `top-packages.mjs`) are compared with `cargo xtask package-registrations` against pi's registrations (`packages.mjs`). Each package installs and loads in a child process of its own on both sides, with file access limited to its scratch directory, no process or network grant on ri's side, and the same five environment variables. 443 of the 475 comparable packages register the same as in pi, or 93%. pi itself fails on 25 under the sandbox: six platform-specific binaries, and the rest blocked by Node's permission model. Of the 32 that differ:

- Native addons or WebAssembly (12): context-mode, opencode-codebase-index, open-codebase-index and @shanepadgett/tau-agent, and eight that pi fails on too (sharp, libsql, wreq-js, the parcel watcher, imagescript).
- pi internals beyond the extension API (7): pi-fabric patches `ExtensionRunner`; @gotgenes/pi-subagents uses `createToolSearchExtension`; pi-multi-codex and pi-plus use `builtinProviders`; pi-landstrip and pi-smart-router construct `SettingsManager` and `ModelRuntime`; pi-llama-cpp fails the same way.
- Loads in ri where pi fails (2): pi-harness-runtime, @amaster.ai/pi-task-scheduler. Both fail with different errors (3): doompi-workflow, pi-shipd-checks, opl-pi-sht.
- Other (7): pi-crew and pi-retry register fewer tools and commands; pi-free registers an extra `glob`; pi-docparser and pi-memory differ in tool details; pi-vertex-claude needs Node's deprecated `punycode`; supi-code-intelligence imports a type as a value.
- Install (1): @runfusion/fusion depends on an `npm:` alias, which ri's npm client does not resolve yet.

The comparison found and fixed: `import.meta.resolve`, `process.report`, NUL characters in sources, pi-ai's subpath modules and API provider registry, `stream` and `EventEmitter` as callable constructors, file descriptors and file streams, TypeScript enums, `realpath` under restricted roots, package resolution for npm and git sources, `net` address checks and `http` agents, `node:sea` and `node:sqlite` names, CommonJS scripts without module syntax, imports with queries, and TypeScript grammar checks.

Nine scenarios recorded from pi match: an extension tool called by the model with a result handler, a prompt message and a flag (JSON mode); a command that sends a message (RPC mode); load failures; an unknown flag; a package directory whose manifest names its extension; and the package commands (local install, list, empty list, removing an unknown package). The npm client is tested against a mock registry.

Not yet done:

- `message_end` replacements, `tool_call` handlers that change the call's input, and the `before_provider_request`, `user_bash`, `model_select`, `resources_discover` and `session_before_*` events.
- Command context actions that replace the session (`newSession`, `fork`, `navigateTree`, `switchSession`, `reload`), and completions through pi-ai from extensions.
- `registerProvider` with a custom `streamSimple`, an OAuth sign-in or `refreshModels`; providers configured like `models.json` entries work.
- `session_start` always reports the reason `startup`; `pi.sendUserMessage` expands prompt templates.
- Boundary events: `turn_end` handlers cannot stage entries or continue the run, and `agent_before_settle` is not sent; `turn_end` carries only `turnIndex`, `message` and `toolResults`.
- Package update checks at startup; registry credentials from `.npmrc`; temporary installs for `-e npm:` and `-e git:` sources; `ri config` installing configured packages that are missing.
- Asynchronous host operations (timers, processes, HTTP) in the Rust SDK.

## M6: extension UI

Done for the slice below.

- `ExtensionUi` covers pi's `ExtensionUIContext`: dialogs with timeouts, the editor dialog, footer statuses, widgets above and below the editor, a replaced footer and header, the terminal title, the working message, indicator and visibility, the hidden thinking label, the editor's text, pasting, tool expansion and the theme.
- Remote components: pi-tui components stay in the extension runtime by handle. The TUI paints the lines of their last render and asks for a new render when they are stale, so a frame never waits for JS. Keys go to the focused component; `tui.requestRender()` marks components stale.
- `ctx.ui.custom` in the editor's place or as an overlay (pi-tui's layout and compositing), component widgets, `setFooter` and `setHeader` factories, tool `renderCall` and `renderResult` (with `lastComponent`, shared state and `renderShell: "self"`) and message renderers. Custom messages with `display: true` show in the transcript.
- The theme reaches extensions as escape sequences per token, so `theme.fg`, `theme.bg` and the facade's list, editor, settings and markdown themes produce pi's output.
- Extension commands' argument completions (`getArgumentCompletions`) in the editor; the guest answers in the background and the list opens when it does.
- The startup listing names loaded extensions, skills and prompts as pi does, compactly or grouped by scope with paths; resources keep the source and scope of the settings entry, package or flag that named them, which also sets their autocomplete tags.
- pi's `[Skill conflicts]` and `[Prompt conflicts]` sections: names taken twice, skills breaking the Agent Skills name and description rules (a skill without a description does not load), and missing or unusable paths.
- RPC mode sends `extension_ui_request` lines for statuses, widgets (lines only), the title, the editor text and the editor dialog, and passes dialog timeouts.

Exit criterion. 24 TUI scenarios recorded from pi match row for row at 80×24 and 120×40: widgets and footer statuses during a turn, pi's timed confirm and select examples, select, confirm, input and editor dialogs open and answered, a custom component open and answered, custom messages with and without pi's message renderer example, the editor text, pi's `question.ts` tool driven by the model, with its custom component and its call and result renderers, and pi's `overlay-test.ts` overlay with wide characters, emoji and inline input. An RPC scenario matches pi's `extension_ui_request` lines, themed status text included.

Not yet done:

- Overlays: `nonCapturing` and `visible` options, and `OverlayHandle` focus and visibility changes.
- `onTerminalInput`, `setEditorComponent`, `addAutocompleteProvider` and `setTheme`; dialog `signal` options.
- Entry renderers (`registerEntryRenderer`); extension keybindings inside components use pi-tui's defaults, not `keybindings.json`.
- A theme change reaches extensions when their session next starts.
- The `/mcp` manager.

## M7: codemode, budgets, import, release

Done for the slice below.

- Codemode: the `codemode` tool, registered inactive, runs each script in a fresh `ri-js` instance without grants or file access. The script gets a new QuickJS context holding only pi's prelude (vendored unchanged), so `tools`, `ALL_TOOLS`, `searchTools`, `describeTool`, `describeNamespace`, `text`, `image`, `console`, `exit`, `store` and `load` behave and fail as in pi. Results carry pi's header, error summaries, output budget with a temp file, and `details.calls`; `store()` writes become `codemode-store` entries. `@options` timeouts and aborts stop the instance at once.
- Nested tool calls (`ctx.executeTool`) for codemode and JS extensions: pi's ids, `tool_execution_*` events with `parentToolCallId`, `tool_call` and `tool_result` hooks, and `nestedCalls` with summed usage on the tool result. Bash and MCP tools declare pi's output schemas, so scripts receive their structured results.
- While codemode is active, declared tools say how scripts call them and its description lists the callable tools that are not declared, grouped by namespace within `codemode.inlineBudget`, with TypeScript declarations rendered from their schemas.
- MCP servers with `codemode` exposure activate codemode, honoring `autoEnableCodemode`. The API facade's `createCodemodeExtension` registers the same tool for packages that decorate it, and a package's own `codemode` replaces the built-in.
- Interactive mode draws codemode calls and results as pi does and leaves nested calls out of the transcript.
- `ri import pi` copies pi's agent directory (settings, credentials, models, keybindings, MCP servers, trust, system prompt files, sessions, prompts, skills, themes, extensions and packages) and the current project's `.pi` into ri's, keeping files ri already has.
- A tag-triggered release workflow builds stripped binaries for Linux and macOS on x86_64 and arm64, with checksums.

Budgets, from `cargo xtask bench --pi` (release build, 100×40 terminal; keystrokes in a session of about 10,000 transcript lines; memory sampled 2 s after first paint; the 10 extensions each register a tool, a command and an event handler). Linux is this machine (x86_64). macOS is GitHub's hosted `macos-latest` runner (arm64 VM), measured by the Bench workflow, which runs for a pushed commit whose message contains `[bench]`; the column shows its last run.

| Metric | Budget | ri, Linux | pi, Linux | ri, macOS | pi, macOS |
|---|---|---|---|---|---|
| `--version` | < 5 ms | 2.0 ms | 246 ms | 5.8 ms (missed) | 204 ms |
| Print mode, start to first request byte | < 25 ms | 13.1 ms | 336 ms | 10.8 ms | 323 ms |
| Interactive first paint | < 40 ms | 10.0 ms | 330 ms | 22.7 ms | 270 ms |
| Keystroke to paint, p99 | < 16 ms | 1.9 ms | 6.1 ms | 6.7 ms | 9.7 ms |
| Idle memory, no extensions | < 30 MB | 15.5 MiB | 106 MiB | 14.4 MiB | 119 MiB |
| Idle memory, 10 JS extensions | < 70 MB | 28.6 MiB | 110 MiB | 26.2 MiB | 124 MiB |
| Stripped release binary | < 35 MB | 33.6 MB | | 26.5 MB | |

The hosted macOS runner is noisier than Linux. Across five runs, `--version` measured 4.5 to 6.7 ms, while starting `true` took 1.3 to 1.7 ms; first paint met its budget in four of the five runs and measured 56 ms in the other. The `--version` overhead is the dynamic loader loading and initializing Security and CoreFoundation before `main`. A probe on the same runner timed a plain Rust binary at the cost of `true`, and the same binary linked against those two frameworks 2.1 ms slower, the same as `ri --version`. ri links them only through `rustls-platform-verifier`, which reqwest uses on every rustls build to check certificates against the system trust store.

The wasm engine starts only when an extension loads or a codemode script runs, so sessions without them do not pay for it.

Exit criterion. Scripts cannot reach files, processes, the network, the environment, modules or host natives: the sandbox tests check every global a script sees against pi's list and that imports fail (`crates/ri-ext/tests/codemode.rs`, which also covers output, errors, the store, limits and discovery). Three scenarios recorded from pi match: a script with sequential nested calls and the store (JSON mode, with requests and the session file), a script error, and the interactive rendering.

Not yet done:

- `codemode.mode: "only"`, which hides direct tools from requests; ri treats it as `on`.
- The `models` global (classifiers and image generation) and grammar-constrained sampling of scripts on the Responses wire APIs.
- The warning pi prints when a package's `codemode` replaces the built-in.
- `--version` within its budget on macOS, which needs ri to stop linking Security and CoreFoundation; see the budgets above.

## Final end-to-end pass

Run on Linux x86_64 after M7, on the release candidate at the head of this branch.

| Check | Command | Result |
|---|---|---|
| Live differential against pi `v1.0.0` | `cargo xtask e2e --differential` | All 239 scenarios match: 93 TUI (PTY, 80×16 to 400×40), 50 JSON mode (six replaying live OpenCode Go streams), 77 CLI and print mode, 18 RPC, and pi's `RpcClient` example driving ri. They cover every wire API, sessions, compaction, the TUI, sign-in, MCP, packages, extensions with their UI, and codemode. |
| Workspace tests | `cargo test --workspace` | 296 tests pass, including the scenario suite against the recorded goldens. |
| QA review | Agents drove ri and pi side by side on the same inputs and compared screens, styles, requests, files and exit codes | Seven areas: the CLI, TUI rendering, slash commands, extensions, print, JSON and RPC modes, sign-in and MCP, and the editor. Every finding is fixed, with a regression test or a scenario recorded from pi, or listed in [compat.md](../docs/compat.md). |
| pi's example extensions | `crates/ri-ext/tests/examples.rs` | 79 of 79 register as in pi. |
| Top 500 npm pi packages | `cargo xtask package-registrations` | 443 of 475 comparable packages register as in pi (93%); see M5. |
| Codemode sandbox | `crates/ri-ext/tests/codemode.rs` | Scripts see exactly pi's globals and reach no files, processes, network, environment, modules or host natives. |
| Budgets | `cargo xtask bench --pi`, here and in the Bench workflow on `macos-latest` | All met on Linux. On macOS all but `--version` (4.5 to 6.7 ms against 5 ms); see M7. |
| Lints, licenses, runtime artifact | `cargo clippy`, `cargo deny check`, `cargo xtask js-runtime --check` | Clean. |

Deferred work is listed under each milestone; intentional differences are in [compat.md](../docs/compat.md).

One QA finding is unresolved: ri exited with SIGABRT, after writing all of its output, in 2 of about 190 RPC runs with an extension loaded. It did not recur in 365 further runs, and no core dump has been captured yet.

## Pending live checks

These need the user's credentials and run outside CI:

- OAuth sign-in with real subscription accounts: Anthropic, ChatGPT (Codex and `openai`), GitHub Copilot, Kimi Code, Meta, xAI, OpenRouter, Radius.
- Requests to Amazon Bedrock, Google Vertex AI and Cloudflare with real credentials.
- A llama.cpp router: `/llama` loads, unloads and downloads, and a classifier request.
