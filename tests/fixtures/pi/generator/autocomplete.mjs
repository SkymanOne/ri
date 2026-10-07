// Records pi-tui's CombinedAutocompleteProvider over a generated directory
// tree, for crates/yapi-tui/tests/autocomplete.rs. `@` cases need `fd` on PATH.
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(join(here, "node_modules/@earendil-works/pi-coding-agent/package.json"));
const dist = pathToFileURL(require.resolve("@earendil-works/pi-tui")).href.replace(/index\.js$/, "");
const { CombinedAutocompleteProvider } = await import(`${dist}autocomplete.js`);
const { fuzzyFilter } = await import(`${dist}fuzzy.js`);

const tree = [
	"src/", "src/main.rs", "src/lib.rs", "src/modes/", "src/modes/print.rs", "src/modes/interactive.rs",
	"docs/", "docs/README.md", "docs/guide one.md", "README.md", "Cargo.toml", "Cargo.lock", ".hidden",
	"tests/", "tests/fixtures/", "tests/fixtures/a.json", "notes.txt",
];
const levels = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
const commandSpecs = [
	{ name: "model", description: "Select model (opens selector UI)", argumentHint: "<provider/model>" },
	{ name: "thinking", description: "Set thinking level", argumentHint: "<level>", levels: true },
	{ name: "tree", description: "Navigate session tree (switch branches)" },
	{ name: "new", description: "Start a new session" },
	{ name: "name", description: "Set session display name" },
	{ name: "skill:commit", description: "[u] Write a commit" },
	{ name: "skill:review", description: "Review code" },
	{ name: "quit", description: "Quit pi" },
];
const commands = commandSpecs.map((spec) => ({
	name: spec.name,
	description: spec.description,
	...(spec.argumentHint && { argumentHint: spec.argumentHint }),
	...(spec.levels && {
		getArgumentCompletions: (prefix) => {
			const filtered = fuzzyFilter(levels, prefix, (level) => level);
			return filtered.length === 0 ? null : filtered.map((level) => ({ value: level, label: level }));
		},
	}),
}));

// [lines, cursorLine, cursorCol, force]
const cases = [
	[["/"], 0, 1, false], [["/mo"], 0, 3, false], [["/th"], 0, 3, false], [["/rev"], 0, 4, false],
	[["/skill:c"], 0, 8, false], [["/thinking "], 0, 10, false], [["/thinking hi"], 0, 12, false],
	[["/thinking zz"], 0, 12, false], [["/new x"], 0, 6, false], [["  /na"], 0, 5, false], [["/zzz"], 0, 4, false],
	[["look at src/"], 0, 12, false], [["look at src/m"], 0, 13, false], [["look at ./"], 0, 10, false],
	[["look at ./do"], 0, 12, false], [["see "], 0, 4, false], [[""], 0, 0, true], [["Car"], 0, 3, true],
	[["open (src/mo"], 0, 12, false], [["open \"docs/gu"], 0, 13, false], [["x src/modes/"], 0, 12, false],
	[["/model src/"], 0, 11, true], [["two", "lines src/"], 1, 10, false], [["read"], 0, 4, false],
	[["@"], 0, 1, false, true], [["see @mai"], 0, 8, false, true], [["see @src/"], 0, 9, false, true],
	[["@\"guide"], 0, 7, false, true], [["@read"], 0, 5, false, true], [["@tests/fix"], 0, 10, false, true],
];

const base = mkdtempSync(join(tmpdir(), "yapi-autocomplete-"));
for (const path of tree) {
	if (path.endsWith("/")) mkdirSync(join(base, path), { recursive: true });
	else writeFileSync(join(base, path), "");
}
let fdPath = null;
try {
	fdPath = execFileSync("sh", ["-c", "command -v fd || command -v fdfind"], { encoding: "utf-8" }).trim() || null;
} catch {}
if (!fdPath) throw new Error("fd is required to record @ cases");

const provider = new CombinedAutocompleteProvider(commands, base, fdPath);
const results = [];
for (const [lines, line, col, force, fd] of cases) {
	const suggestions = await provider.getSuggestions(lines, line, col, { signal: new AbortController().signal, force });
	const applied = suggestions
		? provider.applyCompletion(lines, line, col, suggestions.items[0], suggestions.prefix)
		: null;
	results.push({
		lines, line, col, force, fd: fd ?? false, suggestions, applied,
		fileCompletion: provider.shouldTriggerFileCompletion(lines, line, col),
	});
}
rmSync(base, { recursive: true, force: true });

const out = join(here, "..", "autocomplete");
mkdirSync(out, { recursive: true });
writeFileSync(join(out, "cases.json"), `${JSON.stringify({ tree, levels, commands: commandSpecs, cases: results }, null, "\t")}\n`);
console.log(`${results.length} cases`);
