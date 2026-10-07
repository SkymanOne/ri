// Loads each of pi's example extensions and prints what it registers:
// tools, commands, flags and shortcuts. The yapi side of the comparison lives in
// crates/yapi-ext/tests/examples.rs.
//
//   node extensions.mjs > ../extensions/registrations.json
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { discoverAndLoadExtensions } from "@earendil-works/pi-coding-agent";

const here = path.dirname(fileURLToPath(import.meta.url));
const examples = path.join(here, "node_modules/@earendil-works/pi-coding-agent/examples/extensions");
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "yapi-pi-extensions-"));
const agentDir = path.join(scratch, "agent");
const cwd = path.join(scratch, "project");
fs.mkdirSync(agentDir, { recursive: true });
fs.mkdirSync(cwd, { recursive: true });
process.env.PI_CODING_AGENT_DIR = agentDir;
process.env.PI_OFFLINE = "1";

function dump(extension) {
	return {
		tools: [...extension.tools.values()].map(({ definition }) => ({
			name: definition.name,
			label: definition.label ?? null,
			description: definition.description,
			parameters: definition.parameters,
		})),
		commands: [...extension.commands.values()].map((command) => ({
			name: command.name,
			description: command.description ?? null,
		})),
		flags: [...extension.flags.values()].map((flag) => ({
			name: flag.name,
			type: flag.type,
			default: flag.default ?? null,
			description: flag.description ?? null,
		})),
		shortcuts: [...extension.shortcuts.values()].map((shortcut) => ({
			shortcut: shortcut.shortcut,
			description: shortcut.description ?? null,
		})),
		events: [...extension.handlers.keys()].sort(),
	};
}

const result = {};
for (const name of fs.readdirSync(examples).sort()) {
	if (name === "README.md") continue;
	const loaded = await discoverAndLoadExtensions([path.join(examples, name)], cwd, agentDir);
	result[name] = {
		extensions: loaded.extensions.map(dump),
		errors: loaded.errors.map((error) => error.error.replaceAll(examples, "<examples>").split("\n")[0]),
	};
}
fs.rmSync(scratch, { recursive: true, force: true });
// Descriptions may name the scratch directories; keep the output stable.
const text = JSON.stringify(result, null, "\t").replaceAll(agentDir, "<agent>").replaceAll(cwd, "<cwd>");
process.stdout.write(`${text}\n`);
