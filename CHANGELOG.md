# Changelog

Notable changes to yapi, in the format of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). yapi follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-10-06

Yet Another Pi (in Rust) - Fast, Portable, Compatible.

The first release of yapi, a Rust reimplementation of the [Pi](https://github.com/earendil-works/pi) coding agent. It follows Pi `v1.0.0`, reads and writes Pi's files and runs Pi packages unchanged, as a single binary that needs no Node.js.

### Added

- Interactive, print, JSON and RPC modes, with Pi's flags, slash commands and protocols.
- Sessions in Pi's JSONL format, with branching, `/tree`, `/fork`, `/clone` and compaction.
- The terminal interface: Pi's editor, keybindings, themes and selectors.
- Every wire API and provider in Pi's model catalog. Sign-in with API keys or with subscriptions such as Claude Pro and Max, ChatGPT and GitHub Copilot. See [Models and sign-in](https://skymanone.github.io/yapi/models.html).
- Pi packages from npm, git and local paths. Pi extensions run unchanged in a WebAssembly sandbox, including their interface components, every extension event, session actions, custom editors, terminal input listeners, autocomplete providers and providers with their own streams and sign-in.
- Images are resized and converted as Pi does before they reach the model.
- Native extensions in Rust, built as WebAssembly components. `yapi new` creates one.
- Context files, skills, prompt templates and project trust.
- MCP servers over stdio and streamable HTTP, with OAuth sign-in for HTTP servers (`yapi mcp login`), and codemode.
- `yapi import pi`, which copies Pi's settings, credentials, sessions and packages into yapi's directories.
- Documentation for the model. yapi's pages and Pi's docs and examples install with yapi, or download on its first start, so the model can explain yapi and the extension API it implements.

### Known issues

- Requests to Amazon Bedrock, Google Vertex AI and Cloudflare, and the llama.cpp router, are tested against mock servers only. They are not yet checked against the live services.
- CI runs the `x86_64-apple-darwin` archive only under Rosetta on Apple silicon, not on an Intel Mac.
- Native WebAssembly extensions are unstable until yapi 1.0. The WIT world, the Rust SDK and the host requests they use may change in any release before then, and extensions may need to be rebuilt. Pi extensions from npm use Pi's extension API and are not affected.
- [Differences from Pi](https://skymanone.github.io/yapi/compat.html) lists every intentional difference, and its Open Gaps table links an issue for each Pi feature not yet ported.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/SkymanOne/yapi/main/install.sh | sh
```

[Install](https://skymanone.github.io/yapi/install.html) covers cargo-binstall, direct downloads and building from source.

[0.1.0]: https://github.com/SkymanOne/yapi/releases/tag/v0.1.0
