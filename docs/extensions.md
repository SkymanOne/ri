# Extensions

yapi runs two kinds of extensions on one WebAssembly host:

- Pi extensions, written in TypeScript or JavaScript, run unmodified.
- Native extensions, written in Rust and compiled to WebAssembly components. See [Native extensions in Rust](native-extensions.md).

Both kinds load from the same places, ship in the same [packages](packages.md) and register tools, commands, flags and event handlers through the same model.

## Compatibility with Pi extensions

yapi runs extensions written for `@earendil-works/pi-coding-agent` 1.0, including ones that import the older `@mariozechner/*` package names. Compatibility is measured against Pi itself, with both programs loading the same code:

| Test | Result |
|---|---|
| Pi's example extensions | 79 of 79 register the same tools (with schemas), commands, flags, shortcuts and event handlers as in Pi, and fail with Pi's messages where Pi fails |
| The 500 most-downloaded Pi packages on npm | 443 of the 475 that Pi loads in the test sandbox give the same registrations and load errors in yapi (93%). Of the 367 whose extensions load in Pi without errors and register something, 349 register the same in yapi (95%). |
| Extension UI | Dialogs, widgets, footers, overlays, custom components and tool renderers match Pi's screens row for row at 80×24 and 120×40 |

The packages are the 500 with the `pi-package` keyword that npm reported the most monthly downloads for on 4 October 2026 (`tests/fixtures/pi/packages/top500.json`). Each one installs and loads in a sandbox of its own, in Pi and in yapi: file access is limited to a scratch directory, there is no network or process access during loading, and both get the same five environment variables. Pi itself cannot run 25 of them there: 6 publish builds for other platforms only, Node's permission model stops 16, and 3 crash. That leaves 475 to compare.

The 443 that match include 41 packages without extensions and 44 whose extensions fail in both with the same error, which is why the table also gives the stricter figure. Of the 32 that differ in yapi:

- 12 need native addons or WebAssembly, which yapi's runtime cannot load. Pi fails on 8 of them in the sandbox too, with a different error.
- 7 use parts of Pi beyond the extension API, such as its `SettingsManager`, `ModelRuntime` or internal `ExtensionRunner`.
- 2 load in yapi where Pi fails, and 3 fail in both with different errors.
- 7 register something differently, and 1 depends on an npm alias that yapi's npm client does not install yet.

The checks live in `crates/yapi-ext/tests/examples.rs` and `cargo xtask package-registrations`.

## How Pi extensions run

yapi does not need Node.js. TypeScript and JavaScript extensions run in `yapi-js`, a QuickJS-NG runtime compiled to a WebAssembly component.

- yapi resolves modules as Node does and strips TypeScript types on the host, with a cache keyed by file content.
- `@earendil-works/pi-coding-agent`, `pi-ai`, `pi-agent-core`, `pi-tui` and `typebox` resolve to copies bundled with yapi.
- Shims provide Node's `fs` with file descriptors and file streams, `path`, `os`, `child_process`, `events`, `stream`, `util`, `crypto` hashes, `buffer`, timers, `fetch`, `dns.lookup` and `Intl`.
- Custom TUI components render inside the runtime. yapi paints their last frame, so a slow extension never blocks the interface.

Some Node features have no counterpart in the runtime. Native addons, `net` and `tls` servers and `worker_threads` fail when used, and the rest of the extension keeps working. [Differences from Pi](compat.md) lists the details.

## Sandboxing

Each extension instance runs with a memory limit and a compute limit. yapi checks every file, process, network and environment access against the extension's grants. Every package receives Pi's defaults for now, which allow all four, so extensions behave as they do in Pi. Per-package restrictions in settings are planned.

Packages with full grants share one runtime instance. Each native extension gets an instance of its own. A crashed instance restarts and its extensions reload.

## Loading extensions

yapi loads extensions from:

- `~/.yapi/agent/extensions`, and `.yapi/extensions` in a trusted project
- packages and files added with `yapi install`, which accepts npm and git sources, local folders and single `.ts`, `.js` or `.wasm` files
- `-e <path>` for one run, which accepts a file or a package folder

`yapi -ne` starts without extensions.

## Writing a Pi extension

Extensions use Pi's API. Save this as `~/.yapi/agent/extensions/hello.ts`:

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

Start yapi and run `/hello Ada`. Pi's [extension documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/extensions.md) covers tools, events, UI components and the rest of the API.
