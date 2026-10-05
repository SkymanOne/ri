// Loads the most-downloaded npm pi packages (../packages/ranked.json) in pi
// and prints what each registers, for crates' comparison in `cargo xtask
// package-registrations`.
//
//   node packages.mjs > ../packages/registrations.json
//   node packages.mjs <name>...   # measures these again in registrations.json
//
// Packages install with --ignore-scripts. Each loads in its own Node process
// under the permission model: no environment, and file access only to its
// scratch directory and pi's install, so third-party code reaches nothing
// else on the machine. Four packages run at once (JOBS overrides it), and
// each scratch directory is removed once its package is measured.
import { execFile, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const top = JSON.parse(fs.readFileSync(path.join(here, "../packages/ranked.json"), "utf8"));
const scratch = path.join(os.tmpdir(), "ri-pi-packages");
fs.rmSync(scratch, { recursive: true, force: true });

function run(command, args, options) {
	return new Promise((resolve) => {
		execFile(command, args, { maxBuffer: 64 * 1024 * 1024, ...options }, (error, stdout, stderr) =>
			resolve({ error, stdout, stderr }),
		);
	});
}

function load(args, env) {
	return new Promise((resolve) => {
		const child = spawn(process.execPath, args, { env, stdio: ["ignore", "pipe", "pipe"] });
		let stdout = "";
		let stderr = "";
		child.stdout.on("data", (chunk) => (stdout += chunk));
		child.stderr.on("data", (chunk) => (stderr += chunk));
		const timer = setTimeout(() => child.kill("SIGKILL"), 120_000);
		child.on("close", (status) => {
			clearTimeout(timer);
			resolve({ stdout, stderr, status });
		});
	});
}

const only = process.argv.slice(2);
const result = {};
async function measure(index, name, version) {
	const dir = path.join(scratch, String(index));
	const agentDir = path.join(dir, "agent");
	const cwd = path.join(dir, "project");
	const root = path.join(agentDir, "npm");
	fs.mkdirSync(root, { recursive: true });
	fs.mkdirSync(cwd, { recursive: true });
	fs.mkdirSync(path.join(dir, "home"), { recursive: true });
	fs.writeFileSync(path.join(root, "package.json"), `${JSON.stringify({ name: "pi-extensions", private: true }, null, 2)}\n`);
	fs.writeFileSync(path.join(agentDir, "settings.json"), `${JSON.stringify({ packages: [`npm:${name}@${version}`] }, null, 2)}\n`);
	const install = await run(
		"npm",
		["install", `${name}@${version}`, "--prefix", root, "--ignore-scripts", "--legacy-peer-deps", "--no-audit", "--no-fund", "--loglevel=error"],
		{ timeout: 300_000 },
	);
	if (install.error) {
		const lines = String(install.stderr || install.error.message)
			.split("\n")
			.filter((line) => line.trim() && !line.includes("A complete log of this run"));
		result[name] = { version, install: lines.slice(0, 2).join(" ") };
		process.stderr.write(`${name}: install failed\n`);
		fs.rmSync(dir, { recursive: true, force: true });
		return;
	}
	// pi probes ancestors for `.git` and `.agents`; only those paths are readable.
	const probes = [];
	for (let ancestor = path.dirname(dir); ; ancestor = path.dirname(ancestor)) {
		for (const name of [".git", ".agents"]) {
			probes.push(`--allow-fs-read=${path.join(ancestor, name)}`, `--allow-fs-read=${path.join(ancestor, name)}/*`);
		}
		if (ancestor === path.dirname(ancestor)) break;
	}
	const child = await load(
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
		{ PATH: process.env.PATH, HOME: path.join(dir, "home"), TMPDIR: dir, PI_OFFLINE: "1", PI_CODING_AGENT_DIR: agentDir },
	);
	fs.rmSync(dir, { recursive: true, force: true });
	const marker = "@@registrations@@";
	let dump;
	try {
		dump = JSON.parse(child.stdout.slice(child.stdout.lastIndexOf(marker) + marker.length));
		if (!child.stdout.includes(marker)) throw new Error("no registrations");
	} catch {
		dump = { crash: (child.stderr || `exit ${child.status}`).split("\n").slice(0, 3).join(" ") };
	}
	result[name] = { version, ...dump };
	process.stderr.write(`${name}: ${dump.crash ? "crashed" : `${dump.extensions?.length ?? 0} extension(s)`}\n`);
}

const queue = top.map(({ name, version }, index) => ({ index, name, version })).filter(({ name }) => only.length === 0 || only.includes(name));
const workers = Array.from({ length: Number(process.env.JOBS ?? 4) }, async () => {
	for (let next = queue.shift(); next; next = queue.shift()) {
		await measure(next.index, next.name, next.version);
	}
});
await Promise.all(workers);
// pi's own install directory appears in some tool descriptions.
const piPackage = path.dirname(path.dirname(fileURLToPath(import.meta.resolve("@earendil-works/pi-coding-agent"))));
const measured = JSON.parse(
	JSON.stringify(result)
		.replaceAll(piPackage, "<pi-package>")
		.replaceAll(scratch, "<scratch>")
		.replace(/<scratch>\/\d+\/agent/g, "<agent>")
		.replace(/<scratch>\/\d+\/project/g, "<cwd>"),
);
const saved = path.join(here, "../packages/registrations.json");
const merged = only.length > 0 ? { ...JSON.parse(fs.readFileSync(saved, "utf8")), ...measured } : measured;
const ordered = Object.fromEntries(top.filter(({ name }) => name in merged).map(({ name }) => [name, merged[name]]));
const text = `${JSON.stringify(ordered, null, "\t")}\n`;
if (only.length > 0) fs.writeFileSync(saved, text);
else process.stdout.write(text);
