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

- `/settings`, `/scoped-models`, `/trust`; `/login` and `/logout` arrive with OAuth (M4).
- HTML export; changelog entries.
- Clipboard image paste, terminal images, mermaid, mouse selection.
- Regular mode re-renders the whole document each frame, as pi does; fullscreen reuses unchanged rows.

## M4: RPC, MCP, OAuth, remaining wire APIs

In progress.

RPC mode is done for the slice below.

- pi's JSONL protocol: LF framing, every command of `rpc-types.ts`, responses emitted as pi does (the prompt's at preflight, with its disposition), events in the JSON mode shapes, concurrent command handling, session replacement through the startup factory, and exit on end of input, SIGTERM (143) or SIGHUP (129).
- Shared with interactive mode: pi's runtime rules for new, forked, cloned and switched sessions, including forks of sessions that are not saved; pi's model and thinking level switching, which records a level only when it changes; and the queue display, which drops a queued message when it starts.

Exit criterion. Nine scenarios recorded from pi match: prompting and queries, steering, follow-up, clearing the queue, abort, session commands (entries, tree, fork messages, fork, switch, clone, new, naming), user bash commands, and settings, model and thinking commands. pi's `RpcClient` example (`tests/fixtures/pi/generator/rpc-client.mjs`) drives ri to the same output and requests as pi.

Not yet done:

- `export_html` (as `/export` to HTML in M3).
- `get_commands` lists prompt templates and skills; pi also lists its built-in `llama` and `mcp` extension commands.
- `extension_ui_request` events and `extension_ui_response` handling, which need extensions (M5, M6).
- `cycle_model` over scoped models (`--models`, `enabledModels`).

MCP is done for the slice below.

- Client: a port of pi-mcp over stdio and streamable HTTP, with timeouts that progress restarts, cancellation, pagination, resumable event streams and pi's shutdown sequence for server processes.
- `mcp.json` from the agent directory and trusted projects, validated with pi's messages.
- The built-in extension: tools as `mcp__<server>__<tool>` with `direct`, `deferred` and `hidden` exposure and `toolExposure` overrides, the resource tools, the `mcp_servers` system prompt section, the startup wait for direct servers, problem reports, server logs in `mcp.log`, and `/mcp` status, `reconnect`, `login` and `logout` routing.
- The extension runner it needs: the `Extension` trait, a tool registry with pi's activation rules, `tool_search` with pi's BM25 ranking, extension commands from prompts, and notifications in every mode.

Exit criterion. MCP scenarios recorded from pi match: direct tools with text, structured, error, progress, image and resource-link results and the resource tools; deferred tools found and loaded by `tool_search`; and `/mcp` in RPC mode with configuration errors, a failed and a disabled server, reconnect and the subcommand errors. Client tests cover both transports against the test server.

Not yet done:

- `codemode` exposure, pi's default, needs the codemode tool (M7). Until then such servers warn that their tools are unreachable unless `tool_search` is active.
- OAuth sign-in for HTTP servers and `auth.provider` tokens (with OAuth, below).
- The `/mcp` manager in the TUI, which needs extension UI (M6); `/mcp` shows the status instead.
- Servers registered by extensions (`pi.registerMcpServer`), and saving enable and exposure changes, which only the manager makes.
- pi's built-in `llama` extension.
- OAuth subscriptions, and the openai-codex-responses, azure-openai-responses and mistral wire APIs.

## M5 to M7

Not started.

## Pending live checks

These need the user's credentials and run outside CI:

- OpenCode Go recording (Anthropic, Completions and Responses routes), once `opencode.ai` is reachable.
- OAuth sign-in with real subscription accounts (M4).
