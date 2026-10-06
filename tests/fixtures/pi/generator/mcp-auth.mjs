// Writes ../agent/mcp-auth.json as `pi mcp login` does: a connection finds
// that the OAuth test server in ../../mcp (needs python3 on PATH) requires a
// sign-in, then pi signs in with its challenge and a "browser" that follows
// the authorization redirect to pi's callback. Usage: node mcp-auth.mjs
// (Node >= 22.19)

import { spawn } from "node:child_process";
import { copyFileSync, mkdirSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const agentDir = "/tmp/ri-pi-mcp-auth/agent";
rmSync(dirname(agentDir), { recursive: true, force: true });
mkdirSync(agentDir, { recursive: true });
process.env.PI_CODING_AGENT_DIR = agentDir;

const { McpOAuthCredentialStore, McpServerConnection, createDefaultTransport, signInMcpServer } = await import(
	join(here, "node_modules/@earendil-works/pi-coding-agent/dist/extensions/mcp/runtime.js")
);

const server = spawn("python3", [join(here, "../../mcp/server.py"), "--http", "--oauth"], {
	stdio: ["ignore", "pipe", "inherit"],
});
const url = await new Promise((resolve) => createInterface({ input: server.stdout }).once("line", resolve));
const credentials = new McpOAuthCredentialStore();
const connection = new McpServerConnection({
	entry: { name: "demo", config: { url }, source: join(agentDir, "mcp.json"), scope: "global" },
	cwd: agentDir,
	createTransport: createDefaultTransport,
	credentials,
	onTools: () => {},
});
try {
	await connection.getClient().catch(() => {});
	if (connection.state !== "needs-auth") throw new Error(`unexpected state ${connection.state}`);
	await signInMcpServer({
		serverUrl: url,
		store: credentials.forServer("demo", url),
		settings: connection.oauthSettings(),
		challenge: connection.challenge,
		prompt: {
			showAuthorizationUrl: (authorizationUrl) => void fetch(authorizationUrl),
			promptForRedirectUrl: (signal) =>
				new Promise((resolve) => signal.addEventListener("abort", () => resolve(undefined))),
		},
	});
	copyFileSync(join(agentDir, "mcp-auth.json"), join(here, "../agent/mcp-auth.json"));
} finally {
	await connection.close();
	server.kill();
}
