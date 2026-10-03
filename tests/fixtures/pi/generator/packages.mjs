// Loads the top npm pi packages (../packages/top50.json) in pi and prints
// what each registers, for crates' comparison in `cargo xtask
// package-registrations`.
//
//   node packages.mjs > ../packages/registrations.json
//
// Packages install with --ignore-scripts. Each loads in its own Node process
// under the permission model: no environment, and file access only to its
// scratch directory and pi's install, so third-party code reaches nothing
// else on the machine.
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const top = JSON.parse(fs.readFileSync(path.join(here, "../packages/top50.json"), "utf8"));
const scratch = path.join(os.tmpdir(), "ri-pi-packages");
fs.rmSync(scratch, { recursive: true, force: true });

const only = process.argv[2];
const result = {};
for (const [index, { name, version }] of top.entries()) {
	if (only && name !== only) continue;
	const dir = path.join(scratch, String(index));
	const agentDir = path.join(dir, "agent");
	const cwd = path.join(dir, "project");
	const root = path.join(agentDir, "npm");
	fs.mkdirSync(root, { recursive: true });
	fs.mkdirSync(cwd, { recursive: true });
	fs.mkdirSync(path.join(dir, "home"), { recursive: true });
	fs.writeFileSync(path.join(root, "package.json"), `${JSON.stringify({ name: "pi-extensions", private: true }, null, 2)}\n`);
	fs.writeFileSync(path.join(agentDir, "settings.json"), `${JSON.stringify({ packages: [`npm:${name}@${version}`] }, null, 2)}\n`);
	try {
		execFileSync(
			"npm",
			["install", `${name}@${version}`, "--prefix", root, "--ignore-scripts", "--legacy-peer-deps", "--no-audit", "--no-fund", "--loglevel=error"],
			{ stdio: ["ignore", "ignore", "pipe"], timeout: 300_000 },
		);
	} catch (error) {
		result[name] = { version, install: String(error.stderr ?? error.message).split("\n")[0] };
		process.stderr.write(`${name}: install failed\n`);
		continue;
	}
	// pi probes ancestors for `.git` and `.agents`; only those paths are readable.
	const probes = [];
	for (let ancestor = path.dirname(dir); ; ancestor = path.dirname(ancestor)) {
		for (const name of [".git", ".agents"]) {
			probes.push(`--allow-fs-read=${path.join(ancestor, name)}`, `--allow-fs-read=${path.join(ancestor, name)}/*`);
		}
		if (ancestor === path.dirname(ancestor)) break;
	}
	const child = spawnSync(
		process.execPath,
		[
			"--permission",
			`--allow-fs-read=${dir}`,
			`--allow-fs-read=${here}`,
			...probes,
			`--allow-fs-write=${dir}`,
			path.join(here, "load-package.mjs"),
			agentDir,
			cwd,
		],
		{
			env: { PATH: process.env.PATH, HOME: path.join(dir, "home"), TMPDIR: dir, PI_OFFLINE: "1", PI_CODING_AGENT_DIR: agentDir },
			encoding: "utf8",
			timeout: 120_000,
			maxBuffer: 64 * 1024 * 1024,
		},
	);
	let dump;
	try {
		dump = JSON.parse(child.stdout);
	} catch {
		dump = { crash: (child.stderr || `exit ${child.status}`).split("\n").slice(0, 3).join(" ") };
	}
	result[name] = { version, ...dump };
	process.stderr.write(`${name}: ${dump.crash ? "crashed" : `${dump.extensions?.length ?? 0} extension(s)`}\n`);
}
// pi's own install directory appears in some tool descriptions.
const piPackage = path.dirname(path.dirname(fileURLToPath(import.meta.resolve("@earendil-works/pi-coding-agent"))));
const text = JSON.stringify(result, null, "\t").replaceAll(piPackage, "<pi-package>").replaceAll(scratch, "<scratch>").replace(/<scratch>\/\d+\/agent/g, "<agent>").replace(/<scratch>\/\d+\/project/g, "<cwd>");
process.stdout.write(`${text}\n`);
