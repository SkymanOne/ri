# pi golden fixtures

Files written by pi `v1.0.0` (commit `a13d35a`). `crates/yapi-types/tests/golden.rs` checks that yapi reads and writes them back byte-identically, `crates/yapi-core/tests/session.rs` that yapi builds the same model context from each session, `crates/yapi-core/tests/mcp.rs` that yapi's MCP sign-in stores the same state, `crates/yapi-tui/tests/keys.rs` that yapi decodes terminal input as pi does, `crates/yapi-tui/tests/editor.rs` that yapi's editor behaves as pi's, `crates/yapi-tui/tests/theme.rs` that yapi's themes produce pi's colors, and `crates/yapi-tui/tests/text.rs` that yapi lays out text and markdown as pi does. Do not edit them by hand; regenerate instead.

## Regenerate

Requires Node 22.19 or later.

```sh
cd tests/fixtures/pi/generator
npm install --ignore-scripts
node generate.mjs
node contexts.mjs
node keys.mjs
node editor.mjs
node theme.mjs
node text.mjs
node autocomplete.mjs   # needs fd on PATH
node models-api.mjs
node mcp-auth.mjs       # needs python3 on PATH
```

The generator's packages also serve the end-to-end scenarios: `cargo xtask e2e` runs this pi install, and `rpc-client.mjs`, pi's `RpcClient` example, drives the program under test in client scenarios.

The script runs pi offline in `/tmp/yapi-pi-fixtures` with faux providers. Ids and timestamps change on every run. To cut `legacy/` again from pi's own test sessions, set `PI_SOURCE` to a pi `v1.0.0` checkout.

## Contents

| Path | Written by | Covers |
|---|---|---|
| `agent/settings.json` | `SettingsManager` setters | Global settings as pi edits them |
| `agent/settings-all-fields.json` | Generator; loaded by pi | Every `Settings` field |
| `agent/auth.json` | `ModelRuntime.login` | `api_key` (with `env`) and `oauth` credentials |
| `agent/models.json` | Generator; validated by pi | Custom provider, model definition, overrides |
| `agent/keybindings.json` | pi startup migration | Legacy action names rewritten |
| `agent/mcp.json`, `project/mcp.json` | `pi mcp add` | stdio and HTTP servers, global and project scope |
| `agent/mcp-auth.json` | pi's MCP sign-in, as `pi mcp login` runs it, against the OAuth mode of `tests/fixtures/mcp/server.py` | Discovery state, dynamic client registration and tokens of an MCP server |
| `project/settings.json` | `SettingsManager` project setters | Project scope |
| `sessions/main.jsonl` | `AgentSession` with faux providers | Every entry type, message role, content block and stop reason |
| `sessions/{export,branched,forked,child}.jsonl` | Export, branch, fork, `parentSession` | Header variants |
| `legacy/*.v1.jsonl` | Excerpts of pi's `test/fixtures` sessions | v1 input |
| `sessions/legacy-*.jsonl` | pi's v1 to v3 migration of `legacy/` | Real Anthropic and OpenAI messages |
| `contexts/*.json` | `buildSessionContext` on each session | Compaction, branch summaries, context edits, model and thinking level |
| `keys/keys.json` | pi-tui `matchesKey`, `parseKey`, `decodePrintableKey` over generated input | Legacy, Kitty and Windows Terminal key decoding; the Kitty and Windows modes list only inputs that decode differently from legacy |
| `keys/input.json` | pi-tui `StdinBuffer` fed chunk by chunk | Sequence splitting, partial sequences, pastes |
| `theme/theme.json` | pi's built-in themes in both color modes; `generateSystemThemeColors` | Every token's escape sequence; system themes for no report, black, white, mid-gray and palette terminals |
| `text/text.json` | pi-tui `wrapTextWithAnsi`, `truncateToWidth` and `Markdown` with an identity theme | Wrapping, truncation and markdown blocks at three widths, with and without preserved list markers and escapes |
| `editor/editor.json` | pi-tui `Editor` driven key by key | Text, cursor, rendered rows and submissions after every key: wrapping, word motion, kill ring, undo, history, pastes and markers, sticky columns, jumps, scrolling |
| `models-api/cases.json` | pi-ai's `pi-messages`, System One, llama.cpp classifier and OpenRouter image APIs, with `fetch` stubbed | Request URLs, headers and bodies, results and error messages; checked by `crates/yapi-ai/tests/models_api.rs` |
| `autocomplete/cases.json` | pi-tui `CombinedAutocompleteProvider` over a generated tree | Slash commands and arguments, skill names, path and quoted completion, `@` search through `fd`, applying the first item |

The excerpts in `legacy/` come from pi, Copyright (c) 2025 Mario Zechner, MIT License.
