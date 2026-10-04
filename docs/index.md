# ri

ri is a Rust implementation of [pi](https://github.com/earendil-works/pi), a minimal, extensible AI agent for the terminal. Give it a task and a working folder, and it reads files, runs commands and edits code until the task is done.

ri follows pi `v1.0.0`. It uses pi's commands, settings, session files, packages and extension API, so a pi setup works in ri as it is. It ships as one native binary and needs no Node.js.

## Why ri

- It starts in about 2 ms and paints its first screen in 10 ms. pi takes 246 ms and 330 ms on the same machine.
- It idles at about 15 MB of memory, against 106 MB for pi.
- pi extensions run unchanged inside a WebAssembly sandbox, next to native extensions written in Rust.

## Compatibility

ri is tested against pi itself. Every check below runs both programs on the same input.

| Check | Result |
|---|---|
| pi's example extensions | 79 of 79 register the same tools, commands, flags and shortcuts as in pi |
| The 500 most-downloaded pi packages on npm | 443 of 475 comparable packages install and register as in pi (93%) |
| End-to-end scenarios | 239 of 239 match pi, including 93 terminal screens |
| Settings, credentials and session files | Read and written back byte for byte |

[Differences from pi](compat.md) lists every intentional difference.

## Next steps

- New to ri: [install it](install.md) and follow the [Quickstart](quickstart.md).
- Using pi already: read [Coming from pi](migrating.md).
- Extending ri: start with [Extensions](extensions.md).

ri is pre-release software. Report problems in the [issue tracker](https://github.com/SkymanOne/ri/issues).
