# AGENTS.md

Project plan and working rules for `yapi`, for human and agent contributors.

## Purpose

`yapi` is a Rust reimplementation of the [pi](https://github.com/earendil-works/pi) coding agent. Goals, in priority order:

1. Run pi packages and read pi's on-disk formats unchanged.
2. Start faster, use less memory and install smaller than pi.
3. Provide a sandboxable extension system built on WebAssembly components.

pi `v1.0.0` (commit `a13d35a`) is the behavioral specification. Intentional deviations are listed in [docs/compat.md](docs/compat.md).

License: MIT OR Apache-2.0. Vendored pi JS keeps its MIT notices.

## Scope

### In scope for v0.1

- Agent loop with parallel tool execution, steering and follow-up queues.
- Built-in tools `read`, `bash`, `edit`, `write`, and optional `grep`, `find`, `ls`.
- Sessions as a JSONL v3 tree, with branching, `/fork`, `/clone`, `/tree` and compaction.
- Modes: interactive, print, JSON, RPC.
- pi-ai wire APIs with the data-driven model catalog. Authentication by API key or OAuth subscription (Claude Pro/Max, ChatGPT/Codex, GitHub Copilot).
- Context files (`AGENTS.md`, `CLAUDE.md`), skills, prompt templates, themes, keybindings, project trust.
- Package installation from npm, git and local paths.
- Native WebAssembly extensions, and unmodified pi npm extensions including custom TUI components.
- MCP client (stdio, streamable HTTP) and codemode.

### Out of scope

- pi's experimental stack (`chord`, `durable`, `protocol`, `client`, `server`) and `evals`.
- Native addons, `net`/`tls` servers and `worker_threads` in npm packages.
- Windows as a tier-1 platform. Linux and macOS are tier 1; Windows is best-effort.

### Compatibility contract

| Surface | Rule |
|---|---|
| `settings.json`, `auth.json`, `models.json`, `keybindings.json`, `mcp.json`, session files | Same formats as pi. Round-trip is byte-identical. |
| Locations | `~/.yapi/agent` (override: `YAPI_CODING_AGENT_DIR`) and project `.yapi/`. `yapi import pi` copies pi state. |
| CLI flags, slash commands, JSON and RPC protocols | Same names and payloads as pi. |
| Extension API | `ExtensionAPI` as exported by `@earendil-works/pi-coding-agent` 1.0. |

## Glossary

| Term | Meaning |
|---|---|
| Golden | Output recorded from pi `v1.0.0` that a test compares with yapi's: pi's files in `tests/fixtures/pi`, and pi's normalized scenario results in `tests/fixtures/scenarios`. |
| Cassette | An HTTP exchange with a provider, recorded or hand-written, that `yapi-mock` replays. Kept in `tests/fixtures/cassettes`. |
| Scenario | One end-to-end run of a program against a cassette in a fresh directory, listed in `tests/fixtures/scenarios/scenarios.json`. |
| Differential | Running pi and yapi live on the same scenario and comparing them directly, with `cargo xtask e2e --differential`. |
| Codemode | pi's tool that runs a model-written script calling other tools, in an instance with no file, process or network access. |
| Slice | The part of a milestone delivered and tested end to end. `dev/status.md` states each milestone's slice and what remains. |

## Architecture

A Cargo workspace whose crates mirror pi's packages, so every behavior traces back to its pi source.

| Path | Role | pi counterpart |
|---|---|---|
| `crates/yapi-types` | Serde types for every pi JSON shape: messages, events, session entries, settings, models, extension payloads. | Types across packages |
| `crates/yapi-ai` | Wire APIs, model catalog and registry, cost, credentials, OAuth. | `pi-ai` |
| `crates/yapi-agent` | Agent loop, tool execution, queues, events. | `pi-agent-core` |
| `crates/yapi-tui` | ratatui-based widgets, editor, raw input decoder, ANSI bridge. | `pi-tui` |
| `crates/yapi-core` | Session manager, compaction, settings, resource loading, built-in tools, system prompt, packages, extension runner, MCP. | `pi-coding-agent` core |
| `crates/yapi-ext` | wasmtime host, WIT bindings, capability grants, JS module loader, codemode, embedded `yapi-js.wasm`. | Extension loader, `pi-codemode` |
| `crates/yapi` | Binary: CLI, modes, UI adapters, and the native extension package template that `yapi new` and `cargo generate` use. | `pi-coding-agent` CLI and modes |
| `crates/yapi-mock` | Mock provider server: replays HTTP cassettes and records requests. Test support only. | None |
| `guest/yapi-js` | JS runtime component: QuickJS-NG, Node shims, pi API facade, vendored pi JS. | None |
| `guest/yapi-extension-api` | Rust SDK for native extensions. | None |
| `wit/` | Versioned WIT packages shared by host and guests. | None |
| `install.sh`, `scripts/` | The release installer, and the packaging and install check that the Release workflow runs. | None |
| `skills/` | Agent Skills for using yapi and writing its extensions, with a template and helper scripts. The repository installs as a package that provides them. | None |
| `xtask/` | Developer commands: mock server runner, end-to-end scenarios, benchmarks, model catalog codegen, pi JS vendoring, `yapi-js.wasm` build. | Build scripts |

Dependency direction:
- `yapi-types` ← `yapi-ai` ← `yapi-agent` ← `yapi-core` ← `yapi-ext` ← `yapi`.
- `yapi-tui` ← `yapi`.
- `yapi-core` never depends on wasmtime.
- `guest/` is a separate workspace, so `cargo build` never compiles wasm. The SDK's optional `widgets` feature takes `yapi-tui` and `yapi-types` from this workspace by path.
- `yapi-mock` is only ever a dev-dependency.

### Boundary traits

Traits exist only where implementations vary across a crate or plugin boundary. Closed sets such as package sources, modes and wire API ids are enums. Everything else is a concrete type. Inside a crate, prefer generics; at registries, use `dyn Trait`.

| Trait | Crate | Responsibility | Implementors |
|---|---|---|---|
| `Provider` | `yapi-ai` | Stream an assistant message for a model and context. | One per wire API; extension-registered providers |
| `OAuthProvider` | `yapi-ai` | Log in, refresh, derive an API key from credentials. | Built-in subscriptions; extension-registered providers |
| `Tool` | `yapi-agent` | Declare a schema; execute with progress updates and cancellation. | Built-in, MCP and extension tools |
| `AgentHooks` | `yapi-agent` | Intercept the loop: context transform, before and after tool calls, queue reads. | `yapi-core` session, which dispatches to extensions |
| `Extension` | `yapi-core` | Register capabilities; handle events, tool calls and commands; build components for the transcript items it draws; refresh the model lists of providers it registers. | Built-ins (MCP); wasm instances in `yapi-ext` |
| `Bridge` | `yapi-ext` | Answer an instance's requests and operations that need the session. | The session bridge of loaded extensions; `NoBridge` for headless loading |
| `ExtensionUi` | `yapi-core` | Dialogs, notifications, widgets, custom components, extension editors, input listeners and autocomplete providers. | Interactive TUI; RPC (`extension_ui_request`); headless no-op |
| `ComponentHost` | `yapi-core` | Render an extension component for a width; deliver input and editor operations to it; run the runtime's terminal input listeners and autocomplete providers. | Extension runtimes in `yapi-ext` |

Add a trait only when a second implementation or a plugin boundary exists.

### Stack

| Concern | Choice |
|---|---|
| Async, HTTP, CLI | `tokio`, `reqwest` with rustls, `clap` for commands pi does not have (`yapi new`, `yapi import`, `xtask`). Pi's own arguments and commands keep ports of pi's hand-written parsers, whose quirks and messages clap cannot reproduce. |
| Serialization | `serde`, `serde_json` |
| TUI | `ratatui-core` text and buffer types, `rustix` termios for raw mode, `ratatui-image` |
| Markdown, diffs, images | `pulldown-cmark`, `similar`, `image` |
| Syntax highlighting | `syntect` or tree-sitter, not chosen yet, weighed by binary size |
| Wasm host | `wasmtime` with the component model |
| Guest JS engine | `rquickjs` (QuickJS-NG) |
| TypeScript and resolution | `oxc_transformer`, `oxc_resolver` |
| MCP | A port of `pi-mcp`, so requests and errors match pi's |
| Logging, errors | `tracing`; `thiserror`, `anyhow` |
| Tests | pi goldens, `yapi-mock` (on `hyper`), `portable-pty` and `vt100` for terminal scenarios |

## Extension system

Two tiers share one host, one WIT world and one package format.

- **Native.** WebAssembly components built with `guest/yapi-extension-api` for `wasm32-wasip2`, following Zed's model.
- **pi compat.** Unmodified pi npm extensions run inside `yapi-js`, a single QuickJS-NG runtime component. It contains:
  - Node built-in shims;
  - vendored pi JS (`pi-tui`, `typebox`);
  - a facade for `@earendil-works/pi-coding-agent`, `pi-ai` and `pi-agent-core`, including the legacy `@mariozechner/*` names.

### Packages

- Manifests use the `package.json` `pi` key. An optional `yapi` key with the same shape takes precedence in yapi.
- Entries dispatch by file type: `.ts` and `.js` go to `yapi-js`, `.wasm` goes to the native host.
- Installation uses a built-in npm registry client: semver, integrity checks, nested `node_modules`, no lifecycle scripts. pi's `npmCommand` setting overrides it.
- Git sources use the `git` CLI.
- Native addons are installed but never built. An extension fails only when it loads one.

### Module loading

Module loading runs on the host, in `yapi-ext`:
- `oxc_resolver` handles Node resolution and `oxc_transformer` handles TypeScript, with a content-hash transpile cache.
- pi host packages resolve to the vendored modules.

### WIT

- Typed WIT carries the mechanics: lifecycle, calls and op resolution, render, input, errors.
- pi API calls and events travel as JSON defined in `yapi-types`, the same shapes used by RPC mode and session files.
- Each released world is frozen in `wit/since_vX.Y.Z/`, and the host keeps one binding module per version.
- The target is wasip2. wasip3 async arrives later as a new WIT version.

### Trust and capabilities

- One wasm instance per trust domain:
  - all full-trust packages share one `yapi-js` instance;
  - each package with restricted grants gets its own;
  - codemode gets a fresh instance with no grants.
- Default grants match pi: filesystem, process, network.
- Users restrict a package in settings. Planned: v0.1 gives every package the default grants.
- The host enforces grants: WASI preopens for files; grant checks for exec, fetch and environment.
- Each instance runs under epoch interruption and a memory limit. A trapped instance is restarted and its extensions reloaded.

### Concurrency

- One actor thread owns each instance's wasmtime `Store`.
- The guest is a reactor. Each export runs until the microtask queue drains, then returns its outcomes.
- Async host work (process output, fetch, timers, dialogs) is an op that completes through a later `resolve` call. The Rust SDK runs guest futures on a single-threaded executor that `resolve` drives.
- Host imports never call into a guest synchronously. Anything that could re-enter is queued.

### UI bridge

- Components render ANSI lines inside the guest: pi-tui components in `yapi-js`, and the Rust SDK's `Component` in native extensions, over the same `render` and `input` exports and `ui.*` requests.
- The TUI paints a remote component's cached lines, parsed from ANSI by `yapi-tui`, and maps pi's cursor marker to the frame cursor.
- The TUI requests renders for dirty handles and paints the last result, so it never waits on the guest.
- Inside the guest, pi-tui reports a terminal without hyperlink or image support, so components use pi-tui's own text fallbacks.
- Known losses are listed in [docs/compat.md](docs/compat.md).

Feasibility study, prior art and rejected alternatives: [dev/research/extension-compat.md](dev/research/extension-compat.md).

## Design decisions

| # | Decision | Rationale |
|---|---|---|
| 1 | All extensions are wasm components hosted by wasmtime. | Memory isolation, resource limits and enforceable grants; one host for both tiers. |
| 2 | One QuickJS-NG runtime component for pi packages. | A wasm build per package is impractical. rquickjs ships wasip2 bindings, and pi already uses QuickJS on WASI for codemode. |
| 3 | No Node sidecar in v0.1. | yapi must not require a Node install. A sidecar can be added later behind `Extension`. |
| 4 | JSON payloads over typed WIT mechanics. | One schema in `yapi-types` serves sessions, RPC and extensions. WIT versions change only when mechanics change. |
| 5 | Host-side module loader. | Native-speed transforms, a smaller guest, and codemode carries no compiler. |
| 6 | Built-in npm client that skips lifecycle scripts. | No Node dependency. Lifecycle scripts mostly build native addons, which the wasm runtime cannot load. |
| 7 | Instances are trust domains. | Grants cannot be enforced between packages that share one JS realm. |
| 8 | pi-tui's line model on ratatui text types, with an ANSI bridge. | Components render styled lines for a width, as in pi-tui, so regular mode keeps pi's scrollback redraw and extension components map one to one. ratatui supplies styled text and test buffers. Extension components lose only clickable links and inline images, which fall back to text. |
| 9 | Raw input decoder ported from pi's `keys.ts`. | JS components expect raw terminal bytes in `handleInput`. crossterm's parser discards them. |
| 10 | pi formats in yapi's own directories. | Package and session compatibility without two tools writing one directory. |
| 11 | `yapi-js.wasm` is committed with an inputs hash. | Plain `cargo build` needs no wasm toolchain. CI rejects stale artifacts. |
| 12 | Linux and macOS are tier 1. | Windows specifics (PowerShell tool, console input) follow once the core is stable. |
| 13 | pi JSON files are order-preserving documents; `yapi-types` structs are views over them. | pi's key order depends on the code path and on user edits, so only the document round-trips byte-identically. |
| 14 | The binary embeds `yapi-js.wasm` deflated, and the release profile optimizes Cranelift's code generator for size. | Both are used only when the component cache has no entry, once per runtime version. Together they save 4.8 MB for about 0.2 s more on that one compile. |
| 15 | The docs the model reads, yapi's book and pi's README, docs and examples, are a `yapi-docs.tar.gz` release asset, not files in the repository or the binary. install.sh unpacks it into `~/.yapi/agent/docs`, other installs download it on their first run, and without a local copy the prompt points to the published docs. | pi's system prompt, header and sign-in help point to the docs pi installs with its package. Packing pi's files from its npm tarball at release time keeps them out of the repository and the binary. |

## Performance

`cargo xtask bench` measures yapi against pi on the same machine: startup (interactive first paint, and print mode to the first request byte), keystroke latency, memory and binary size. Reports state the results as measured, with no targets.
- Runs alternate between yapi and pi, and reports give the median with the range of the samples.
- MB means 10^6 bytes.
- [docs/performance.md](docs/performance.md) describes each measure and the method.

## Development guidelines

### Workflow

- Read the pi source at `v1.0.0` before implementing any behavior. Clone it outside this repository; do not vendor pi source.
- Before every commit, run:
  - `cargo fmt --all`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
  - `cargo deny check`
- CI runs the same checks on Linux and macOS for pull requests and pushes to `main`.
- After changing a guest, `yapi-tui` or `yapi-types`, rebuild with `cargo xtask js-runtime` and commit the artifacts with their inputs hash. It rebuilds `yapi-js.wasm`, which needs a WASI SDK, only when the runtime's own inputs changed.
- Regenerate the model catalog with `cargo xtask models`. Never edit generated files.
- Serve a cassette to pi or another out-of-process client with `cargo xtask mock-sse --cassette <file>`. It prints the base URL and, on exit, reports requests that did not match. `--record <upstream-url> --out <file>` records a new cassette through a proxy to a real provider; credentials are never written.

### Code

- Rust stable, the `stable` channel in `rust-toolchain.toml`, edition 2024. `rust-version` in the root `Cargo.toml` is the oldest Rust that builds yapi.
- KISS: choose the simplest design that matches pi's behavior. No speculative generality.
- DRY: one definition per concept. pi JSON shapes live only in `yapi-types`.
- Serialize pi JSON only through `yapi_types::json`, which matches `JSON.stringify`. Clippy rejects direct `serde_json::to_*` calls.
- Errors:
  - `thiserror` in libraries; `anyhow` only in `yapi` and `xtask`.
  - No `unwrap` or `expect` outside tests unless the invariant is stated at the call site.
- `#![forbid(unsafe_code)]` in every crate except `yapi-ext` and `guest/*`.
- Log through `tracing`. Stdout belongs to print, JSON and RPC output.
- Async on tokio:
  - no lock held across `.await`;
  - no blocking calls on async threads;
  - no synchronous call into a guest from a host import.
- Doc comments on public items state contracts, not implementation.
- Workspace lints in the root `Cargo.toml` enforce the docs, error and logging rules. Every crate sets `[lints] workspace = true`.

### Dependencies

- Declare dependencies in the root `[workspace.dependencies]`.
- Pin `wasmtime`, `oxc_*` and `rquickjs` to exact versions and upgrade deliberately.
- A new dependency needs a stated reason and its binary-size cost (`cargo bloat --crates`) in the commit message.
- Licenses must be compatible with MIT OR Apache-2.0. `cargo deny` enforces this.

### Testing

- No network access and no real providers in tests. Use the faux provider, or `yapi-mock` with cassettes from `tests/fixtures/cassettes`.
- pi-produced golden fixtures live in `tests/fixtures/pi`; its README explains how to regenerate them. Formats must round-trip byte-identical.
- End-to-end scenarios in `tests/fixtures/scenarios` run a program against a cassette in a fresh directory and a cleared environment:
  - `cargo test` compares yapi's normalized output and requests with goldens recorded from pi;
  - `cargo xtask e2e --record-pi` rewrites the goldens; `cargo xtask e2e --differential` compares live runs;
  - the suite needs `rg` and `fd` on `PATH` for the search tool scenarios;
  - TUI scenarios type into the program in a pseudo-terminal and compare the final screen text, without each product's startup header; `YAPI_SETTLE_MS` lengthens the quiet time that ends each step on slow machines;
  - RPC scenarios send each command once the previous one's response or awaited event has arrived;
  - client scenarios run a Node script from the fixture generator, such as pi's `RpcClient` example, against the program; `cargo test` skips them when the generator's packages are not installed;
  - goldens are recorded on Linux; a scenario whose screen shows text pi chooses by platform, such as macOS's Option key name, sets `"os": "linux"` and runs only there;
  - MCP scenarios and tests connect to the Python test server in `tests/fixtures/mcp`, so they need `python3` on `PATH`.
- Prefer goldens recorded from pi over hand-written expectations. Components with pi counterparts are tested against pi-tui's output (`tests/fixtures/pi/generator`).
- A nightly differential suite runs pinned pi (requires Node) and yapi on the same inputs and compares:
  - event streams;
  - session files;
  - extension registrations;
  - per-row visible text of extension UI.
- Every bug fix includes a regression test.

### Compatibility

- Behavior follows pi `v1.0.0`. Record every intentional deviation in [docs/compat.md](docs/compat.md) with its reason.
- For an upstream bump, diff pi's extension `types.ts` and docs between tags. Update `yapi-types`, the JS facade and the oracle pin in one change.

### Docs and git

- Concise, professional prose. Describe contracts and decisions; do not paraphrase code or diffs.
- Public documentation lives in `docs/` and builds with mdBook from `book.toml`. The Docs workflow publishes it to GitHub Pages from `main`. User-facing prose avoids em dashes and semicolons.
- Internal notes live in `dev/`: progress in `dev/status.md`, research in `dev/research/`.
- A change to user-facing behavior updates the matching page in `docs/` in the same commit.
- A change to the architecture or to a decision updates this file in the same commit.
- Commit messages use `type(scope): summary`. Type is one of `feat`, `fix`, `docs`, `refactor`, `test`, `chore`; scope is the crate name.
- Pull request descriptions state the key changes, behavior changes and decisions. They do not paraphrase commits or report line counts.
- Stage explicit paths. Never commit secrets, `target/` or local caches.

## Milestones

v0.1 is the completion of M7. Progress, deferred work and pending live checks are tracked in [dev/status.md](dev/status.md).

| Milestone | Scope | Exit criteria |
|---|---|---|
| M0 | Workspace, CI on Linux and macOS, `yapi-types`, `xtask`, mock SSE server. | pi golden files round-trip byte-identical. |
| M1 | `yapi-ai` with Anthropic Messages, OpenAI Completions, OpenAI Responses and Google; catalog; cost; API keys; print and JSON modes; `yapi-mock` record mode. | JSON event streams match pi on recorded cassettes: text, thinking, tools, images, abort. |
| M2 | Agent loop, built-in tools, system prompt, context files, session tree, fork and clone, compaction, skills, prompt templates. | Sessions written by either tool open in the other. Scenario suite matches pi. |
| M3 | Interactive TUI: editor, keybindings, themes, selectors, tree view, regular mode. | Snapshot suite green. |
| M4 | Remaining wire APIs, OAuth subscriptions, RPC mode with extension UI, MCP. | pi's `rpc-client` example drives yapi. OAuth checklist passes. MCP fixtures pass. |
| M5 | `yapi-ext`, `yapi-js` without UI, module loader, packages, Rust SDK. | At least 90% of pi's 79 example extensions and of the top 50 npm pi packages register the same tools, commands, flags and shortcuts as in pi. |
| M6 | Extension UI: dialogs, widgets, overlays, renderers, custom editors. | Per-row visible text matches pi at 80×24 and 120×40, except listed deviations. No frame is blocked by JS. |
| M7 | Codemode, performance measurement, `yapi import pi`, release packaging. | Codemode has no file, process or network access. `cargo xtask bench` reports yapi against pi on tier-1 targets. |
