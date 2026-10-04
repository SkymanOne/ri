// pi's examples/rpc-client.ts, driving the program given as the first
// argument instead of pi's dist/cli.js. RpcClient runs `node <cliPath>`, so
// cliPath is this file, which re-runs as a shim that executes the program.
//
//   node rpc-client.mjs <program> [prompt...]
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

if (process.env.RPC_CLIENT_SHIM) {
	const child = spawn(process.env.RPC_CLIENT_SHIM, process.argv.slice(2), { stdio: "inherit" });
	for (const signal of ["SIGTERM", "SIGINT", "SIGHUP"]) {
		process.on(signal, () => child.kill(signal));
	}
	child.on("exit", (code, signal) => process.exit(code ?? (signal === "SIGTERM" ? 143 : 1)));
} else {
	const { RpcClient } = await import("@earendil-works/pi-coding-agent");
	const [program, ...words] = process.argv.slice(2);
	const prompt = words.join(" ") || "Explain this repository in one paragraph.";

	const client = new RpcClient({
		cliPath: fileURLToPath(import.meta.url),
		args: ["--no-session"],
		env: { RPC_CLIENT_SHIM: program },
	});

	const unsubscribe = client.onEvent((event) => {
		if (event.type === "message_update" && event.assistantMessageEvent.type === "text_delta") {
			process.stdout.write(event.assistantMessageEvent.delta);
		} else if (event.type === "tool_execution_start") {
			process.stderr.write(`\n[tool: ${event.toolName}]\n`);
		}
	});

	try {
		await client.start();
		await client.promptAndWait(prompt);
		process.stdout.write("\n");
	} finally {
		unsubscribe();
		await client.stop();
	}
}
