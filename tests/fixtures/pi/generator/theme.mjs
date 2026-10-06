// Records pi's theme colors, for crates/yapi-tui/tests/theme.rs: the escape
// sequence of every token of the built-in themes in both color modes, and the
// system theme generated for several terminal color reports.
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const agent = join(here, "node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/theme");
const { loadThemeFromPath } = await import(pathToFileURL(join(agent, "theme.js")).href);
const { generateSystemThemeColors } = await import(pathToFileURL(join(agent, "system-theme.js")).href);

const BACKGROUNDS = ["selectedBg", "searchMatchBg", "userMessageBg", "customMessageBg", "toolPendingBg", "toolSuccessBg", "toolErrorBg"];

const builtin = {};
for (const name of ["dark", "light"]) {
	for (const mode of ["truecolor", "256color"]) {
		const theme = loadThemeFromPath(join(agent, `${name}.json`), mode);
		const tokens = {};
		for (const token of Object.keys(theme.colors)) {
			tokens[token] = BACKGROUNDS.includes(token) ? theme.getBgAnsi(token) : theme.getFgAnsi(token);
		}
		builtin[`${name}/${mode}`] = tokens;
	}
}

const rgb = (hex) => ({ r: Number.parseInt(hex.slice(1, 3), 16), g: Number.parseInt(hex.slice(3, 5), 16), b: Number.parseInt(hex.slice(5, 7), 16) });
const palette = (hexes) => hexes.map(rgb);
const catppuccin = palette([
	"#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
	"#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
]);
const solarized = palette([
	"#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
	"#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
]);
const gruvbox = palette([
	"#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984",
	"#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
]);

const inputs = {
	"none": { saturation: 1 },
	"none-light-pending": { saturation: 0, appearanceHint: "light" },
	"black": { background: rgb("#000000"), foreground: rgb("#e5e5e7") },
	"white": { background: rgb("#ffffff"), foreground: rgb("#000000") },
	"mid-gray": { background: rgb("#808080"), foreground: rgb("#ffffff") },
	"catppuccin": { background: rgb("#1e1e2e"), foreground: rgb("#cdd6f4"), palette: catppuccin },
	"solarized-light": { background: rgb("#fdf6e3"), foreground: rgb("#657b83"), palette: solarized },
	"gruvbox-pending": { background: rgb("#282828"), foreground: rgb("#ebdbb2"), palette: gruvbox, saturation: 0 },
	"background-only": { background: rgb("#1a1b26") },
};
const system = {};
for (const [name, input] of Object.entries(inputs)) {
	system[name] = { input, ...generateSystemThemeColors(input) };
}

const out = join(here, "..", "theme");
mkdirSync(out, { recursive: true });
writeFileSync(join(out, "theme.json"), `${JSON.stringify({ builtin, system }, null, "\t")}\n`);
console.log(`${Object.keys(builtin).length} built-in theme modes, ${Object.keys(system).length} system themes`);
