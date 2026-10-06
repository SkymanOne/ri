# Extensions

yapi runs two kinds of extensions on one WebAssembly host:

- Pi extensions, written in TypeScript or JavaScript, run unmodified.
- Native extensions, written in Rust and compiled to WebAssembly components. See [Native extensions in Rust](native-extensions.md).

Both kinds load from the same places, ship in the same [packages](packages.md) and register tools, commands, flags and event handlers through the same model.

Native WebAssembly extensions are unstable until yapi 1.0. The WIT world, the Rust SDK and the host requests they use may change in any release before then, and extensions may need to be rebuilt. Pi extensions from npm use Pi's extension API and are not affected.

## Compatibility with Pi extensions

yapi runs extensions written for `@earendil-works/pi-coding-agent` 1.0, including ones that import the older `@mariozechner/*` package names. Compatibility is measured against Pi itself, with both programs loading the same code:

| Test | Result |
|---|---|
| Pi's example extensions | 79 of 79 register the same tools (with schemas), commands, flags, shortcuts and event handlers as in Pi, and fail with Pi's messages where Pi fails |
| The 500 most-downloaded Pi packages on npm | 418 of 500 work without errors |
| Extension UI | Dialogs, widgets, footers, overlays, custom components and tool renderers match Pi's screens row for row at 80×24 and 120×40 in the recorded scenarios, except for the [listed differences](compat.md#extensions) |

A package works without errors when its extensions load in yapi without errors and register the same tools, commands, flags, shortcuts and event handlers as in Pi. 43 of the 418 have no extensions and provide only skills, prompts or themes.

47 of the other 82 fail in Pi too, with the same error. Most of the remaining 35 need native addons, parts of Pi beyond the extension API, or Node features the runtime lacks. [dev/status.md](https://github.com/SkymanOne/yapi/blob/main/dev/status.md#m5-extension-host-headless) describes how the packages were chosen and lists each difference.

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
