# Extensions

ri runs two kinds of extensions on one WebAssembly host:

- pi extensions, written in TypeScript or JavaScript, run unmodified.
- Native extensions, written in Rust and compiled to WebAssembly components. See [Native extensions in Rust](native-extensions.md).

Both kinds load from the same places, ship in the same [packages](packages.md) and register tools, commands, flags and event handlers through the same model.

## Compatibility with pi extensions

ri runs extensions written for `@earendil-works/pi-coding-agent` 1.0, including ones that import the older `@mariozechner/*` package names. Compatibility is measured against pi itself:

| Test | Result |
|---|---|
| pi's example extensions | 79 of 79 register the same tools (with schemas), commands, flags, shortcuts and event handlers as in pi, and fail with pi's messages where pi fails |
| The 50 most-downloaded pi packages on npm | 46 of 48 comparable packages install with ri's npm client and register as in pi (96%) |
| Extension UI | Dialogs, widgets, footers, overlays, custom components and tool renderers match pi's screens row for row at 80×24 and 120×40 |

pi itself fails to load two of the 50 packages under the test harness, which leaves 48 to compare. The two that differ in ri:

- `context-mode` loads `better-sqlite3`, a native Node addon, when it starts.
- `pi-fabric` patches pi's internal `ExtensionRunner`.

The checks live in `crates/ri-ext/tests/examples.rs` and `cargo xtask package-registrations`.

## How pi extensions run

ri does not need Node.js. TypeScript and JavaScript extensions run in `ri-js`, a QuickJS-NG runtime compiled to a WebAssembly component.

- ri resolves modules as Node does and strips TypeScript types on the host, with a cache keyed by file content.
- `@earendil-works/pi-coding-agent`, `pi-ai`, `pi-agent-core`, `pi-tui` and `typebox` resolve to copies bundled with ri.
- Shims provide Node's `fs`, `path`, `os`, `child_process`, `events`, `util`, `crypto` hashes, `buffer`, timers, `fetch` and `Intl`.
- Custom TUI components render inside the runtime. ri paints their last frame, so a slow extension never blocks the interface.

Some Node features have no counterpart in the runtime. Native addons, `net` and `tls` servers and `worker_threads` fail when used, and the rest of the extension keeps working. [Differences from pi](compat.md) lists the details.

## Sandboxing

Each extension instance runs with a memory limit and a compute limit. ri checks every file, process, network and environment access against the extension's grants. Every package receives pi's defaults for now, which allow all four, so extensions behave as they do in pi. Per-package restrictions in settings are planned.

Packages with full grants share one runtime instance. Each native extension gets an instance of its own. A crashed instance restarts and its extensions reload.

## Loading extensions

ri loads extensions from:

- `~/.ri/agent/extensions`, and `.ri/extensions` in a trusted project
- packages and files added with `ri install`, which accepts npm and git sources, local folders and single `.ts`, `.js` or `.wasm` files
- `-e <path>` for one run, which accepts a file or a package folder

`ri -ne` starts without extensions.

## Writing a pi extension

Extensions use pi's API. Save this as `~/.ri/agent/extensions/hello.ts`:

```typescript
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  pi.registerCommand("hello", {
    description: "Show a greeting",
    handler: async (name, ctx) => {
      ctx.ui.notify(`Hello, ${name || "world"}!`, "info");
    },
  });
}
```

Start ri and run `/hello Ada`. pi's [extension documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/extensions.md) covers tools, events, UI components and the rest of the API.
