# Running pi extensions in WebAssembly

| | |
|---|---|
| Status | Accepted |
| Date | 2026-10-02 |
| Reference | pi `v1.0.0` (commit `a13d35a`) |

## Question

Can unmodified pi extensions, distributed as npm packages, be wrapped in WebAssembly and loaded by a Rust host?

## Verdict

Yes, with one qualification: the wasm unit is the JS runtime, not the package.

- **A wasm module per npm package is impractical.**
  - Every module would embed its own engine and the same shims.
  - Packages also expect one shared module graph with pi's host packages.
- **One JS runtime component can load each package's JS unchanged.** The runtime is QuickJS-NG through rquickjs, built for `wasm32-wasip2`. It needs three supporting pieces:
  - Node built-in shims over host imports;
  - JS copies of pi's host packages;
  - a module loader that resolves `node_modules` and transforms TypeScript.
- **Unsupported:** native addons, `net`/`tls` servers, `worker_threads`. None of pi's 79 example extensions use them.

## What pi extensions require

Measured on `packages/coding-agent/examples/extensions`: 79 extensions, of which 70 are single files and 9 are directories. Counts are the number of extensions that use each feature.

| Requirement | Count | Implication |
|---|---|---|
| Custom TUI components: `ctx.ui.custom`, tool renderers, header, footer, editor, widget factories, message and entry renderers | 29 | UI compatibility is a requirement, not an extra. |
| Value imports from `@earendil-works/pi-tui` | 25 | pi-tui classes must exist inside the runtime. Three extensions subclass `CustomEditor`. |
| `typebox` | 12 | Vendored TypeBox. Tool schemas cross the boundary as plain JSON Schema. |
| `path` / `fs` / `child_process` / `os` | 13 / 11 / 9 / 3 | Core shims. |
| `url`, `util`, `zlib`, `readline`, `module` | 4 | Long tail. |
| `net`, `tls`, `http`, `worker_threads` | 0 | Safe to reject. |
| Timers / `fetch` / raw `process.stdout.write` | 14 / 3 / 3 | Host ops. Raw writes go to the terminal writer. |
| Third-party npm dependencies | 4 | Node resolution and CJS interop are required. For example, `ms` is CommonJS. |

The following semantics shape the boundary. Sources are `src/core/extensions/{loader,runner,types}.ts` and `packages/tui/src/tui.ts`.

- **Loading.**
  - jiti performs a full TypeScript transform with ESM/CJS interop.
  - Host packages are injected as virtual modules: `typebox`, `@sinclair/typebox`, `@earendil-works/pi-{agent-core,ai,tui,coding-agent}`, and the legacy `@mariozechner/*` aliases.
- **In-place mutation.** Handlers run sequentially in load order and modify the event object directly. This applies to `tool_call.input`, provider headers, `systemPromptOptions` and `context` messages.
- **Live objects.** `ctx.sessionManager`, `ctx.modelRegistry` and the TUI instance are live host objects, not copies.
- **Synchronous rendering.**
  - `render(width): string[]` and `handleInput(data)` are synchronous.
  - Rendered lines carry SGR styles, OSC 8 links (pi-tui's Markdown emits them), image escapes, and a cursor marker (`\x1b_pi:c\x07`).
- **RPC precedent.** pi's RPC mode already serializes dialogs, notifications, status and string widgets (`extension_ui_request`). It does not serialize custom components.

## Options

| Option | Node API | Outcome | Notes |
|---|---|---|---|
| QuickJS-NG via rquickjs, `wasm32-wasip2` | Shims written by us | Chosen | rquickjs ships wasip1 and wasip2 bindings. Shim code is plain Rust and can be unit-tested natively. |
| Javy (QuickJS) | None | Rejected | Modules are at least 869 KB static, or 1–16 KB with dynamic linking. Output is a core module with a fixed interface, not a component with our WIT. |
| StarlingMonkey (SpiderMonkey) with ComponentizeJS and jco-std | jco-std Node 24 layer | Revisit | The most standards-aligned path, but componentization takes a single ES module. A WASI 0.3 version of the Node layer is blocked upstream (jco#2130). |
| Boa (pure Rust) | None | Rejected | Memory-safe and above 90% ECMAScript coverage per its README, but it has no Node layer. |
| rquickjs embedded natively | Shims written by us | Rejected | The pi_agent_rust approach. Fast, but there is no memory isolation and grants cannot be enforced. |
| deno_core / V8 | Partial | Rejected | Adds tens of MB of V8, and Deno's Node layer is hard to use outside `deno_runtime`. |
| Node or Bun sidecar process | Full | Deferred | Highest compatibility, but it requires Node or Bun on the machine. It can be added behind the `Extension` trait. |
| WASIX runtimes (Wasmer) | Varies | Rejected | wasmtime does not implement WASIX. |

## Prior art

### Zed

The template for the native tier. ri adopts Zed's WIT versioning, build target, epoch limits and default-grant model.
- Extensions are components built for `wasm32-wasip2` (`crates/extension/src/extension_builder.rs`).
- Each released WIT world is frozen in `crates/extension_api/wit/since_vX.Y.Z/`. Ten versions exist so far, from `since_v0.0.1` to `since_v0.8.0`, and the host has bindings for each.
- The host enables epoch interruption (`crates/extension_host/src/wasm_host.rs`).
- Capabilities are granted by default and narrowed through the `granted_extension_capabilities` setting.

### pi_agent_rust

A Rust port that runs pi extensions in QuickJS embedded natively, without wasm, using Node shims and a persistent transpile cache.

Results:
- The README reports 95.6% of 226 extensions passing, regenerated 2026-08-17, and 206 of 208 must-pass extensions.
- An earlier table in its `EXTENSIONS.md` shows 187 of 223 (83.9%). npm-registry packages passed at only 64.0%.
- The failures it lists include multi-file dependencies ("needs bundling") and missing npm stubs.
- Conformance is measured mostly at registration level.

Scope decisions:
- Custom TUI rendering is a non-goal ("core owns the UI").
- The project has dropped strict drop-in parity as "both impractical and undesirable".

Lessons for ri:
- Invest in module resolution early.
- Use real pi as a differential oracle. pi_agent_rust compares its runtime against a Bun-based pi harness.
- Treat UI compatibility as a first-class requirement rather than leaving it out.

### pi itself

`pi-codemode` already runs QuickJS-NG on WASI (`quickjs-wasi`) as a sandbox whose only capability is calling tools. ri's codemode reuses `ri-js` with no grants.

### Other agents

goose and Codex CLI extend through MCP servers and external processes. Neither runs JS in-process.

## Chosen design

`ri-ext` hosts native extension components and one JS runtime component, `ri-js`, on wasmtime. Mechanics and rationale are in [AGENTS.md](../../AGENTS.md#extension-system).

## Risks

| Risk | Mitigation |
|---|---|
| Node API long tail and npm dependency graphs. | Prioritize shims by measured use across a package corpus. The Node-style resolver with CJS interop lands in M5. Unknown modules fail with a clear "unsupported module" error. |
| Synchronous rendering against a wasmtime `Store` that cannot be re-entered. | One actor per instance. The TUI paints cached lines. Host imports never re-enter a guest. |
| QuickJS gaps: `Intl.Segmenter`, which pi-tui's editor uses, and the `Intl` formatters. | Guest polyfills that share ratatui's `unicode-width` tables. |
| ANSI bridge losses: OSC 8 links and image escapes. | pi-tui in the guest reports no hyperlink or image support (its `TerminalCapabilities`), so components use their text fallbacks. Remaining escapes are stripped. Listed in [compat.md](../../docs/compat.md). |
| QuickJS is an interpreter and slower than V8 on CPU-heavy code. | Wizer snapshot of the vendored modules, render caching, bytecode cache. A sidecar remains possible. |
| Upstream API drift. | Pin the pi version per ri release. Diff the extension `types.ts` on every bump. Pin the oracle to the same version. |
| First-run compile latency for `ri-js.wasm`. | Lazy instantiation, a cached `.cwasm`, and optionally precompiled artifacts in releases. |

## Platform status

Accessed 2026-10-02.
- WASI 0.3.0 was released on 2026-06-11. It adds `async func`, `stream<T>` and `future<T>` to the component model.
- Wasmtime 46.0.0 (2026-06-22) states: "Wasmtime now supports WASI 0.3.0 by default and the `component-model-async` wasm feature is now enabled by default." The current release is 49.0.1 (2026-09-24).
- ri targets wasip2 now. Async exports and streams become a new WIT version once guest toolchains for wasip3 are stable.

## Sources

Accessed 2026-10-02.

**pi**
- [pi v1.0.0](https://github.com/earendil-works/pi/tree/v1.0.0): `packages/coding-agent/docs/extensions.md`, `docs/rpc-extension-ui.md`, `src/core/extensions/`, `packages/tui/src/tui.ts`

**Zed**
- [Zed extension API WIT](https://github.com/zed-industries/zed/tree/main/crates/extension_api/wit)
- [Zed extension host](https://github.com/zed-industries/zed/blob/main/crates/extension_host/src/wasm_host.rs)
- [Zed extension builder](https://github.com/zed-industries/zed/blob/main/crates/extension/src/extension_builder.rs)
- [Zed capabilities](https://github.com/zed-industries/zed/blob/main/docs/src/extensions/capabilities.md)

**pi_agent_rust**
- [pi_agent_rust](https://github.com/Dicklesworthstone/pi_agent_rust)
- [pi_agent_rust EXTENSIONS.md](https://github.com/Dicklesworthstone/pi_agent_rust/blob/main/docs/planning/EXTENSIONS.md)

**JS engines and tooling**
- [rquickjs](https://github.com/DelSkayn/rquickjs)
- [Javy](https://github.com/bytecodealliance/javy)
- [StarlingMonkey](https://github.com/bytecodealliance/StarlingMonkey)
- [ComponentizeJS](https://github.com/bytecodealliance/ComponentizeJS)
- [jco#2130: jco-std Node layer for WASI 0.3](https://github.com/bytecodealliance/jco/issues/2130)
- [Boa](https://github.com/boa-dev/boa)
- [oxc](https://github.com/oxc-project/oxc)

**WASI and wasmtime**
- [WASI 0.3 launch](https://bytecodealliance.org/articles/WASI-0.3)
- [Wasmtime 46.0.0 release notes](https://github.com/bytecodealliance/wasmtime/blob/release-46.0.0/RELEASES.md)
- [Wasmtime releases](https://github.com/bytecodealliance/wasmtime/releases)

**ratatui**
- [ratatui viewports](https://github.com/ratatui/ratatui/blob/main/ratatui-core/src/terminal/viewport.rs)
- [ansi-to-tui](https://github.com/ratatui/ansi-to-tui)
