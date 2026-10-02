# pi golden fixtures

Files written by pi `v1.0.0` (commit `a13d35a`). `crates/ri-types/tests/golden.rs` checks that ri reads and writes them back byte-identically, and `crates/ri-core/tests/session.rs` that ri builds the same model context from each session. Do not edit them by hand; regenerate instead.

## Regenerate

Requires Node 22.19 or later.

```sh
cd tests/fixtures/pi/generator
npm install --ignore-scripts
node generate.mjs
node contexts.mjs
```

The script runs pi offline in `/tmp/ri-pi-fixtures` with faux providers. Ids and timestamps change on every run. To cut `legacy/` again from pi's own test sessions, set `PI_SOURCE` to a pi `v1.0.0` checkout.

## Contents

| Path | Written by | Covers |
|---|---|---|
| `agent/settings.json` | `SettingsManager` setters | Global settings as pi edits them |
| `agent/settings-all-fields.json` | Generator; loaded by pi | Every `Settings` field |
| `agent/auth.json` | `ModelRuntime.login` | `api_key` (with `env`) and `oauth` credentials |
| `agent/models.json` | Generator; validated by pi | Custom provider, model definition, overrides |
| `agent/keybindings.json` | pi startup migration | Legacy action names rewritten |
| `agent/mcp.json`, `project/mcp.json` | `pi mcp add` | stdio and HTTP servers, global and project scope |
| `project/settings.json` | `SettingsManager` project setters | Project scope |
| `sessions/main.jsonl` | `AgentSession` with faux providers | Every entry type, message role, content block and stop reason |
| `sessions/{export,branched,forked,child}.jsonl` | Export, branch, fork, `parentSession` | Header variants |
| `legacy/*.v1.jsonl` | Excerpts of pi's `test/fixtures` sessions | v1 input |
| `sessions/legacy-*.jsonl` | pi's v1 to v3 migration of `legacy/` | Real Anthropic and OpenAI messages |
| `contexts/*.json` | `buildSessionContext` on each session | Compaction, branch summaries, context edits, model and thinking level |

The excerpts in `legacy/` come from pi, Copyright (c) 2025 Mario Zechner, MIT License.
