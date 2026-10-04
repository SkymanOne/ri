// Child of packages.mjs: resolves the configured package as pi does, loads
// its extensions and prints what they register after a marker line, since
// extensions may print to stdout too.
import { DefaultPackageManager, SettingsManager, discoverAndLoadExtensions } from "@earendil-works/pi-coding-agent";

const [agentDir, cwd] = process.argv.slice(2);

function dump(extension) {
	return {
		tools: [...extension.tools.values()].map(({ definition }) => ({
			name: definition.name,
			label: definition.label ?? null,
			description: definition.description,
			parameters: definition.parameters,
		})),
		commands: [...extension.commands.values()].map((command) => ({ name: command.name, description: command.description ?? null })),
		flags: [...extension.flags.values()].map((flag) => ({
			name: flag.name,
			type: flag.type,
			default: flag.default ?? null,
			description: flag.description ?? null,
		})),
		shortcuts: [...extension.shortcuts.values()].map((shortcut) => ({ shortcut: shortcut.shortcut, description: shortcut.description ?? null })),
		events: [...extension.handlers.keys()].sort(),
	};
}

const settingsManager = SettingsManager.create(cwd, agentDir);
const packages = new DefaultPackageManager({ cwd, agentDir, settingsManager });
const resolved = await packages.resolve();
const entries = resolved.extensions.filter((resource) => resource.enabled).map((resource) => resource.path);
const loaded = await discoverAndLoadExtensions(entries, cwd, agentDir);
process.stdout.write(
	`\n@@registrations@@${JSON.stringify({
		entries,
		extensions: loaded.extensions.map(dump),
		errors: loaded.errors.map((error) => String(error.error).split("\n")[0]),
	})}`,
);
