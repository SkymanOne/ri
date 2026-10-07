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

Inside yapi, `/mcp` opens a manager that lists the servers with their state, tool count, exposure and configuration file. Servers that need attention come first. Select a server to see its tools and connection details, reconnect, sign in or out, change its exposure, or enable and disable it. Exposure and enabled changes are saved to the `mcp.json` that defines the server, and the rest of the file is kept. Outside the interactive mode, `/mcp` prints the status, and `/mcp login`, `/mcp logout` and `/mcp reconnect` act on a server directly.

## Servers from extensions

An extension adds a server for the session with `pi.registerMcpServer(name, config)`, where `config` takes the same settings as an entry in `mcp.json`. The server connects when the session starts, or right away when registered later, and `pi.unregisterMcpServer(name)` disconnects it. A server of the same name in `mcp.json` takes precedence, and `/mcp` notes the registration it overrides. Exposure and enabled changes to a registered server in `/mcp` apply to the session only and are never saved.

A stdio server needs the extension's process grant and an HTTP server its network grant. Without it, `registerMcpServer` throws an error that names the missing grant. Every package has both grants by default, as in Pi.

## Signing in

HTTP servers without an `Authorization` header sign in with OAuth. A server that asks for a sign-in shows as needing one in `/mcp` and `yapi mcp list`. Select it in `/mcp` or run `/mcp login <server>` inside yapi, or run `yapi mcp login <server>` in a terminal. `/mcp logout <server>` and `yapi mcp logout <server>` delete the stored credentials.

yapi opens the authorization page in your browser and waits for it to redirect back. If the browser runs on another machine, paste the URL it was redirected to. yapi registers itself with the server when the server allows it. For servers that do not, set a pre-registered client in the server's `oauth` object (`clientId`, `clientSecret`, `callbackPort` or `callbackUrl`, `scope`, `clientName`, `authServerMetadataUrl`), or pass `--oauth-client-id` and the related options to `yapi mcp add`.

Credentials live in `~/.yapi/agent/mcp-auth.json`, in the same format as Pi's `mcp-auth.json`, and `yapi import pi` copies Pi's sign-ins. yapi refreshes an access token shortly before it expires and when the server rejects it, so a sign-in lasts as long as the server keeps the grant. A running session picks up a sign-in made with `yapi mcp login` on its next turn.

To send the token of a provider you signed in to with `/login` instead, add `"auth": { "provider": "<provider>" }` to a server in the global `mcp.json`. The token is read for every request, so it follows the provider's refreshes.

## Codemode

By default, yapi offers MCP tools through codemode. Instead of one tool call per step, the model writes a short JavaScript program that calls several tools and combines their results. Programs run in a fresh WebAssembly sandbox with no file, process or network access of their own.

Set `"exposure": "direct"` on a server to offer its tools to the model directly. `--tools read,bash,edit,write,codemode` routes the built-in tools through codemode as well. With `"codemode": { "mode": "only" }` in `settings.json`, the tools that scripts can call are offered only through codemode while it is active. Requests leave out their declarations and the system prompt does not list them.

A package that registers its own `codemode` tool replaces the built-in one, and yapi warns about it at startup.

Scripts also reach the `models` global: they list the catalog and run classifier and image models with the session's credentials. The model reads how to use it in Pi's codemode documentation, in `~/.yapi/agent/docs/pi/docs/codemode.md` when the documentation for the model is installed, else on GitHub.

Pi's [MCP](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/mcp.md) and [codemode](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/codemode.md) documentation describes every option.
