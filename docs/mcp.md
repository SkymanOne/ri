# MCP servers and codemode

yapi connects to [Model Context Protocol](https://modelcontextprotocol.io) servers over stdio and streamable HTTP. Describe them in `~/.yapi/agent/mcp.json`, or in `.yapi/mcp.json` for one project:

```json
{
  "mcpServers": {
    "filesystem": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
    "docs": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" } }
  }
}
```

## Managing servers

`yapi mcp` edits the file and checks servers without starting a session:

```sh
yapi mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem .
yapi mcp add docs --url https://example.com/mcp --bearer-token-env-var DOCS_TOKEN
yapi mcp list                 # state, tools and errors, exits 1 if a server fails
yapi mcp remove docs
```

Inside yapi, `/mcp` shows the same status and `/mcp reconnect` restarts a server.

## Codemode

By default, yapi offers MCP tools through codemode. Instead of one tool call per step, the model writes a short JavaScript program that calls several tools and combines their results. Programs run in a fresh WebAssembly sandbox with no file, process or network access of their own.

Set `"exposure": "direct"` on a server to offer its tools to the model directly. `--tools read,bash,edit,write,codemode` routes the built-in tools through codemode as well.

Scripts also reach the `models` global: they list the catalog and run classifier and image models with the session's credentials. yapi writes the script reference the model reads to `~/.yapi/agent/docs/codemode.md` when codemode is active.

Pi's [MCP](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/mcp.md) and [codemode](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/codemode.md) documentation describes every option.
