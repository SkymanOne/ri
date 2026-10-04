// Builds resource trees and records what pi's `DefaultPackageManager.resolve`
// finds in each: every extension, skill, prompt and theme with its metadata
// and whether it is enabled. `crates/ri-core/tests/resolve.rs` rebuilds the
// same trees and compares ri's resolver with these files.
//
//   node resolve.mjs > ../resolve/cases.json
//
// Paths are written relative to each case's root, as `<root>/...`.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { DefaultPackageManager, SettingsManager } from "@earendil-works/pi-coding-agent";

const scratch = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "ri-pi-resolve-")));

/** Each case: files to write under the root, and whether the project is trusted. */
const cases = {
	"auto-overrides": {
		files: {
			"agent/extensions/a.ts": "",
			"agent/extensions/b.ts": "",
			"agent/extensions/tool/index.ts": "",
			"agent/extensions/tool/helper.ts": "",
			"agent/extensions/notes.txt": "",
			"agent/skills/review/SKILL.md": "",
			"agent/skills/top.md": "",
			"agent/skills/nested/deep/SKILL.md": "",
			"agent/skills/nested/loose.md": "",
			"agent/prompts/fix.md": "",
			"agent/prompts/.hidden.md": "",
			"agent/prompts/sub/inner.md": "",
			"agent/themes/night.json": "{}",
			"agent/settings.json": JSON.stringify({
				extensions: ["-extensions/a.ts"],
				skills: ["!review"],
				prompts: ["+prompts/fix.md", "-prompts/fix.md"],
			}),
		},
	},
	"package-filters": {
		files: {
			"agent/pkg/extensions/x.ts": "",
			"agent/pkg/extensions/y.ts": "",
			"agent/pkg/skills/k/SKILL.md": "",
			"agent/pkg/prompts/p.md": "",
			"agent/pkg/themes/t.json": "{}",
			"agent/other/extensions/z.ts": "",
			"agent/other/prompts/q.md": "",
			"agent/settings.json": JSON.stringify({
				packages: [
					{ source: "./pkg", extensions: ["-extensions/x.ts"], skills: [], prompts: ["prompts/*.md"] },
					{ source: "./other", prompts: ["!prompts/q.md"] },
				],
			}),
		},
	},
	"package-manifest": {
		files: {
			"agent/pkg/package.json": JSON.stringify({
				name: "pkg",
				pi: { extensions: ["./src/*.ts", "!./src/skip.ts"], skills: ["./skills"], themes: [] },
			}),
			"agent/pkg/src/one.ts": "",
			"agent/pkg/src/skip.ts": "",
			"agent/pkg/src/.dot.ts": "",
			"agent/pkg/skills/s/SKILL.md": "",
			"agent/pkg/prompts/ignored-by-manifest.md": "",
			"agent/pkg/themes/t.json": "{}",
			"agent/single.ts": "",
			"agent/settings.json": JSON.stringify({ packages: ["./pkg", "./single.ts"] }),
		},
	},
	"autoload-delta": {
		trusted: true,
		files: {
			"agent/pkg/extensions/x.ts": "",
			"agent/pkg/extensions/y.ts": "",
			"agent/pkg/skills/k/SKILL.md": "",
			"agent/settings.json": JSON.stringify({ packages: ["./pkg"] }),
			"project/.pi/settings.json": JSON.stringify({
				packages: [{ source: "../../agent/pkg", autoload: false, extensions: ["-extensions/x.ts"] }],
			}),
		},
	},
	"top-level-entries": {
		trusted: true,
		files: {
			"agent/more-skills/a/SKILL.md": "",
			"agent/more-skills/draft/b/SKILL.md": "",
			"agent/more-skills/c.md": "",
			"agent/tools/one.ts": "",
			"agent/tools/two.ts": "",
			"agent/settings.json": JSON.stringify({
				skills: ["./more-skills", "!**/draft/**"],
				extensions: ["./tools/one.ts", "./tools", "-tools/two.ts", "-builtin:mcp"],
			}),
			"project/.pi/settings.json": JSON.stringify({ extensions: ["+builtin:mcp"], prompts: ["./local-prompts"] }),
			"project/.pi/local-prompts/lp.md": "",
		},
	},
	"ignore-files": {
		files: {
			"agent/skills/.gitignore": "ignored/\n*.draft.md\n",
			"agent/skills/ignored/SKILL.md": "",
			"agent/skills/kept/SKILL.md": "",
			"agent/skills/notes.draft.md": "",
			"agent/skills/notes.md": "",
			"agent/extensions/.ignore": "skip.ts\n",
			"agent/extensions/skip.ts": "",
			"agent/extensions/keep.ts": "",
			"agent/prompts/.fdignore": "# comment\nold.md\n",
			"agent/prompts/old.md": "",
			"agent/prompts/new.md": "",
		},
	},
	"project-and-agents": {
		trusted: true,
		files: {
			"project/.git/HEAD": "",
			"project/.pi/extensions/p.ts": "",
			"project/.pi/skills/ps/SKILL.md": "",
			"project/.pi/prompts/pp.md": "",
			"project/.pi/themes/pt.json": "{}",
			"project/.agents/skills/as/SKILL.md": "",
			"project/.agents/skills/top.md": "",
			"project/.agents/skills/group/inner.md": "",
			"home/.agents/skills/hs/SKILL.md": "",
			"agent/extensions/u.ts": "",
			"project/.pi/settings.json": JSON.stringify({ skills: ["-../.agents/skills/as"] }),
		},
	},
	"untrusted-project": {
		trusted: false,
		files: {
			"project/.pi/extensions/p.ts": "",
			"project/.pi/settings.json": JSON.stringify({ extensions: ["./x.ts"] }),
			"project/.pi/x.ts": "",
			"agent/extensions/u.ts": "",
		},
	},
};

function relativize(value, root) {
	return typeof value === "string" ? value.split(root).join("<root>") : value;
}

const result = {};
for (const [name, spec] of Object.entries(cases)) {
	const root = path.join(scratch, name);
	for (const [file, text] of Object.entries(spec.files)) {
		const full = path.join(root, file);
		fs.mkdirSync(path.dirname(full), { recursive: true });
		fs.writeFileSync(full, text);
	}
	for (const dir of ["agent", "project", "home"]) fs.mkdirSync(path.join(root, dir), { recursive: true });
	process.env.HOME = path.join(root, "home");
	const cwd = path.join(root, "project");
	const agentDir = path.join(root, "agent");
	const settingsManager = SettingsManager.create(cwd, agentDir, { projectTrusted: spec.trusted ?? false });
	const manager = new DefaultPackageManager({ cwd, agentDir, settingsManager, builtinExtensions: ["mcp"] });
	const resolved = await manager.resolve(async () => "skip");
	const out = { files: spec.files, trusted: spec.trusted ?? false };
	for (const kind of ["extensions", "skills", "prompts", "themes"]) {
		out[kind] = resolved[kind].map(({ path: p, enabled, metadata }) => ({
			path: relativize(p, root),
			enabled,
			source: relativize(metadata.source, root),
			scope: metadata.scope,
			origin: metadata.origin,
			...(metadata.baseDir ? { baseDir: relativize(metadata.baseDir, root) } : {}),
		}));
	}
	result[name] = out;
}
fs.rmSync(scratch, { recursive: true, force: true });
process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
