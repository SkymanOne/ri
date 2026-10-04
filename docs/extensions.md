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
| The 500 most-downloaded pi packages on npm | 443 of 475 comparable packages install with ri's npm client and register the same tools, commands, flags, shortcuts and event handlers as in pi (93%) |
| Extension UI | Dialogs, widgets, footers, overlays, custom components and tool renderers match pi's screens row for row at 80×24 and 120×40 |

The packages are the 500 with the `pi-package` keyword that npm reports the most monthly downloads for. Each one installs and loads in a sandbox of its own, in pi and in ri. pi itself fails to install or load 25 of them there, which leaves 475 to compare. Of the 32 that differ in ri:

- 12 need native addons or WebAssembly, which ri's runtime cannot load. pi fails on 8 of them in the sandbox too.
- 7 use parts of pi beyond the extension API, such as its `SettingsManager`, `ModelRuntime` or internal `ExtensionRunner`.
- 2 load in ri where pi fails, and 3 fail in both with different errors.
- 7 register something differently, and 1 depends on an npm alias that ri's npm client does not install yet.

The checks live in `crates/ri-ext/tests/examples.rs` and `cargo xtask package-registrations`.

## How pi extensions run

ri does not need Node.js. TypeScript and JavaScript extensions run in `ri-js`, a QuickJS-NG runtime compiled to a WebAssembly component.

- ri resolves modules as Node does and strips TypeScript types on the host, with a cache keyed by file content.
- `@earendil-works/pi-coding-agent`, `pi-ai`, `pi-agent-core`, `pi-tui` and `typebox` resolve to copies bundled with ri.
- Shims provide Node's `fs` with file descriptors and file streams, `path`, `os`, `child_process`, `events`, `stream`, `util`, `crypto` hashes, `buffer`, timers, `fetch` and `Intl`.
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
