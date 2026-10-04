# MCP servers and codemode

ri connects to [Model Context Protocol](https://modelcontextprotocol.io) servers over stdio and streamable HTTP. Describe them in `~/.ri/agent/mcp.json`, or in `.ri/mcp.json` for one project:

```json
{
  "mcpServers": {
    "filesystem": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
    "docs": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" } }
  }
}
```

## Managing servers

`ri mcp` edits the file and checks servers without starting a session:

```sh
ri mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem .
ri mcp add docs --url https://example.com/mcp --bearer-token-env-var DOCS_TOKEN
ri mcp list                 # state, tools and errors, exits 1 if a server fails
ri mcp remove docs
```

Inside ri, `/mcp` shows the same status and `/mcp reconnect` restarts a server.

## Codemode

By default, ri offers MCP tools through codemode. Instead of one tool call per step, the model writes a short JavaScript program that calls several tools and combines their results. Programs run in a fresh WebAssembly sandbox with no file, process or network access of their own.

Set `"exposure": "direct"` on a server to offer its tools to the model directly. `--tools read,bash,edit,write,codemode` routes the built-in tools through codemode as well.

pi's [MCP](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/mcp.md) and [codemode](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/codemode.md) documentation describes every option.
