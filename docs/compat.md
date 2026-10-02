# Deviations from pi

Intentional differences from pi `v1.0.0`. Anything not listed here is expected to match pi; a mismatch is a bug.

| Area | pi | ri | Reason |
|---|---|---|---|
| Directories | `~/.pi/agent`, project `.pi/`, `PI_CODING_AGENT_DIR` | `~/.ri/agent`, project `.ri/`, `RI_CODING_AGENT_DIR`; `ri import pi` copies pi state | Same formats without two tools writing one directory. |
| Package manifest | `pi` key | `pi` key, plus an optional `ri` key with the same shape that takes precedence | Lets one package ship a native wasm build for ri and JS for pi. |
| Package installation | Runs `npm install` (or `npmCommand`), which runs lifecycle scripts | Built-in npm client that skips lifecycle scripts; `npmCommand` still overrides it | No Node dependency. Lifecycle scripts mostly build native addons, which ri cannot load. |
| Unsupported packages | Any Node code runs | Packages with native addons, `net`/`tls` servers or `worker_threads` are rejected | Not available inside the wasm runtime. |
| Unpaired UTF-16 surrogates in JSON strings | Read and written as `\udXXX` escapes | A line containing one fails to parse | A Rust `String` cannot hold them. Only malformed text, such as truncated model output, produces them. |
| Extension component output | Uses the terminal's hyperlink (OSC 8) and image support | pi-tui inside ri reports neither, so links print their URL as text and images use pi-tui's text fallback. Escapes emitted directly by extensions are stripped. | ratatui cells cannot carry OSC 8 or image escapes. |
| System prompt | Preamble says "operating inside pi"; a `docs` section points to pi's installed documentation | Preamble says "operating inside ri"; no `docs` section | ri installs no documentation. |
| `--help`, no-models message | Mention `PI_PACKAGE_DIR` and pi's installed docs | Omit both | ri has no package or docs directory. |
| JSON mode `message_update.usage`, assistant `message_start` | Serialized from the live message, so they show state from later in the stream | State at the moment of the event | pi's values depend on stream timing. |
| Terminal input outside the Basic Multilingual Plane | Delivered as two lone UTF-16 surrogates, one per input event | Delivered as one character | Rust strings hold whole characters; the inserted text is the same. |
| Markdown | marked: LaTeX shown as Unicode math, GFM bare URLs become links, code blocks syntax-highlighted | pulldown-cmark: LaTeX shown as written, bare URLs stay plain text, code blocks in the code block color | Not yet ported; text and layout otherwise match pi's renderer. |
| Startup header | pi logo, version, key hints and "Pi can explain its own features and look up its docs" | `ri` wordmark, version and key hints | ri is not pi and ships no documentation for the model to read. |
