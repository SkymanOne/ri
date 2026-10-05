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
| The 500 most-downloaded Pi packages on npm | 418 of 500 work without errors |
| Extension UI | Dialogs, widgets, footers, overlays, custom components and tool renderers match Pi's screens row for row at 80×24 and 120×40 |

A package works without errors when its extensions load in yapi without errors and register the same tools, commands, flags, shortcuts and event handlers as in Pi. 43 of the 418 have no extensions and provide only skills, prompts or themes.

The packages come from npm's ranking of packages with the `pi-package` keyword by monthly downloads: the top 500 on 4 October 2026, followed by the most downloaded on 5 October that were not among them (`tests/fixtures/pi/packages/ranked.json`). Each one installs and loads in a sandbox of its own, in Pi and in yapi: file access is limited to a scratch directory, there is no network or process access during loading, and both get the same five environment variables. Pi itself cannot run 26 of the first 526 there: 6 publish builds for other platforms only, Node's permission model stops 17, and 3 crash. The next packages on the list take their places, so each of the 500 has Pi's result to compare with. A package that fails to install or load in yapi counts against yapi.

Of the 82 that do not work without errors, 47 fail in Pi too, with the same error. 37 of them are parts of doompi that need its host extension loaded first, and the others need a package, program or file that is not installed. The other 35 behave differently in yapi:

- 11 need native addons or WebAssembly, which yapi's runtime cannot load. Pi fails on 7 of them in the sandbox too, with a different error.
- 9 use parts of Pi beyond the extension API, such as `SettingsManager`, `ModelRuntime`, `AuthStorage`, pi-ai's `builtinProviders` or the internal `ExtensionRunner`.
- 3 call `net.getDefaultAutoSelectFamilyAttemptTimeout`, which yapi does not provide. Pi fails on them later, with a different error.
- 3 fail in both with different errors, and 1 loads in yapi where Pi fails.
- 3 fail only in yapi for other reasons: Node's deprecated `punycode` module, Node's `navigator` global, and a type imported as a value.
- 5 register something differently. pi-crew and pi-retry register nothing, pi-free registers an extra tool, and pi-docparser and pi-memory differ in tool details.

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
