# Differences from Pi

yapi follows Pi `v1.0.0`. It reads Pi's settings, sessions, credentials and packages, and behaves like Pi unless this page says otherwise. Any other difference is a bug, and the [issue tracker](https://github.com/SkymanOne/yapi/issues) is the place to report it.

This page lists only differences that change what you can do. Small differences in wording, error messages or product names are not listed.

## Extensions

Pi extensions run unchanged, but not in Pi's Node.js process. yapi runs them in WebAssembly, which limits what they can reach.

| Area | Pi | yapi |
|---|---|---|
| Where extensions run | In Pi's Node.js process, with your permissions | In a QuickJS-NG runtime compiled to WebAssembly. Each call may compute for 60 seconds and each instance may use 1 GiB of memory. An instance that traps restarts and its extensions load again. Grants are full by default, see the [security model](configuration.md#security-model). |
| Node APIs | All of Node | The shims listed in [Extensions](extensions.md#how-pi-extensions-run). These import but throw when used: `http` requests, `net` sockets and servers, `tls`, `worker_threads` workers, `node:sqlite`, `zlib` compression, `vm`, `v8`, `dns` queries other than `lookup`, `http2`, `dgram`, `cluster`, `inspector`, `child_process.fork`, `util.parseArgs`, `stream.pipeline`, `stream.finished`, `events.on` and `process.chdir`. `require()` of an ES module fails. |
| Native addons | Loaded by Node | Installed but not built. Loading one fails. The rest of the package still works if it loads the addon lazily. |
| Pi internals | Extensions can import all of `pi-coding-agent`, `pi-ai` and `pi-agent-core` | The extension API and the helpers extensions use. Other exports import but throw when called, and built-in tool factories reject custom `operations`. |
| `Intl` | ICU in every locale | Number and plural formatting in `en-US` only, and `Intl.DateTimeFormat` ignores its options. |
| Extension UI | Components detect hyperlink and image support, see theme changes at once and match keys against your `keybindings.json` | Links and images in components fall back to pi-tui's text forms, theme changes reach extensions when their session next starts, and components match pi-tui's default key bindings. |
| Console output | Written to stdout and stderr | Written to stderr, because stdout carries print, JSON and RPC output. |
| Project trust | Extensions loaded before trust can answer the `project_trust` event | yapi asks before loading any extension, so no extension runs in a project you have not trusted. |
| Synchronous process timeouts | `execSync` and `spawnSync` send SIGTERM and wait for the process to exit | SIGTERM, then SIGKILL after 5 seconds, as Pi's `exec` does. |
| Codemode scripts | Runaway recursion throws a catchable `RangeError`. A script without `timeout_ms` may compute without limit. | Runaway recursion or 60 seconds of computing without awaiting a call stops the script with a sandbox failure. |
| Native extensions | Not available | yapi also loads WebAssembly components written with its Rust SDK. See [Native extensions in Rust](native-extensions.md). They are unstable until yapi 1.0. |

## Packages and installation

| Area | Pi | yapi |
|---|---|---|
| Directories | `~/.pi/agent`, project `.pi/`, `PI_CODING_AGENT_DIR`, `PI_CODING_AGENT_SESSION_DIR` | `~/.yapi/agent`, project `.yapi/`, `YAPI_CODING_AGENT_DIR`, `YAPI_CODING_AGENT_SESSION_DIR`. The formats are the same, and `yapi import pi` copies Pi's state. |
| Package installs | `npm install`, which runs lifecycle scripts. `npmCommand` replaces npm for every package operation. | A built-in npm client that skips lifecycle scripts and needs no Node.js. `npmCommand` is used only to install and update npm sources. |
| Package manifests | The `pi` key in `package.json` | The `pi` key, or a `yapi` key of the same shape that takes precedence, so one package can ship JavaScript for Pi and a native build for yapi. |
| Self-update | `pi update` updates Pi | `yapi update` updates packages and model catalogs only. Upgrade yapi itself as [Install](install.md#upgrade) describes. |
| `yapi new` | `pi new ...` sends the words as a prompt | Creates a native extension project. A prompt that starts with the word `new` needs quotes. |

## Interface and providers

| Area | Pi | yapi |
|---|---|---|
| `/share` and `/bug` | Publish the session to pi.dev's viewer and send reports to Pi's developers | Not available, because they are Pi's services. |
| Clipboard | A native clipboard addon, then platform commands and OSC 52 | Platform commands and OSC 52. |
| Word motions in Chinese, Japanese and Thai | Move by dictionary words | Move one character at a time, because ICU's word dictionaries would add megabytes to the binary. |
| Tree label times | Local time | UTC, because yapi carries no time zone database. |
| Codex transport | WebSocket first, then SSE with a compressed body | SSE with an uncompressed body. Requests and events are the same as Pi's fallback. |

## Not yet ported

Pi features yapi does not have yet are tracked as [issues labelled `pi-compat`](https://github.com/SkymanOne/yapi/issues?q=is%3Aissue+label%3Api-compat). Each issue says what Pi does and what yapi does today.
