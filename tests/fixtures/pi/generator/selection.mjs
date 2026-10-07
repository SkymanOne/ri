// Records pi-tui's fullscreen mouse selection, for crates/yapi-tui/tests/selection.rs:
// the screen lines after each mouse report sent to `TuiAltScreen`, and the text it copies.
// The layout is pi's chat viewport: a scrolling transcript above a dock.
import { mkdirSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(join(here, "node_modules/@earendil-works/pi-coding-agent/package.json"));
const { ScrollView, TuiAltScreen, VStack, setCapabilities } = await import(
	pathToFileURL(require.resolve("@earendil-works/pi-tui")).href
);
setCapabilities({ images: null, trueColor: true, hyperlinks: false });

// The auto-scroll timer of a held drag runs when a step says `tick`.
const timers = new Set();
globalThis.setInterval = (callback) => {
	const timer = { callback, unref() {} };
	timers.add(timer);
	return timer;
};
globalThis.clearInterval = (timer) => timers.delete(timer);

class Terminal {
	constructor(columns, rows) {
		this.columns = columns;
		this.rows = rows;
	}
	start(onInput) {
		this.onInput = onInput;
	}
	stop() {}
	async drainInput() {}
	write() {}
	get kittyProtocolActive() {
		return true;
	}
	moveBy() {}
	hideCursor() {}
	showCursor() {}
	clearLine() {}
	clearFromCursor() {}
	clearScreen() {}
	setTitle() {}
	setProgress() {}
}

const lines = (rows) => ({ render: () => [...rows], invalidate() {} });
const press = (x, y) => `\x1b[<0;${x};${y}M`;
const drag = (x, y) => `\x1b[<32;${x};${y}M`;
const release = (x, y) => `\x1b[<0;${x};${y}m`;
const click = (x, y) => [press(x, y), release(x, y)];
const numbered = (count) => Array.from({ length: count }, (_, index) => `line ${index + 1}`);

const cases = [
	{
		name: "drag",
		transcript: ["\x1b[1mal\x1b[0mpha", "beta", "gamma", "delta"],
		steps: [press(1, 1), drag(4, 2), release(4, 2)],
	},
	{
		name: "drag backward",
		transcript: ["alpha", "beta", "gamma", "delta"],
		steps: [press(3, 3), drag(2, 1), release(2, 1)],
	},
	{
		name: "single click",
		transcript: ["alpha", "beta", "gamma", "delta"],
		steps: [...click(2, 1), drag(4, 2), release(4, 2)],
	},
	{
		name: "words and lines",
		transcript: ["zero alpha beta", "gamma delta"],
		steps: [
			...click(6, 1),
			...click(10, 1),
			...click(12, 1),
			press(14, 1),
			drag(3, 2),
			release(3, 2),
			...click(7, 2),
			...click(9, 2),
			...click(11, 2),
		],
	},
	{
		name: "paths",
		columns: 60,
		transcript: ["see extensions/starline/fixed-editor/compositor.ts now", "earendil-works/pi-tui"],
		steps: [...click(20, 1), ...click(20, 1), ...click(10, 2), ...click(10, 2)],
	},
	{
		name: "whitespace",
		transcript: ["foo  bar"],
		steps: [...click(1, 1), press(2, 1), drag(4, 1), release(4, 1)],
	},
	{
		name: "graphemes",
		transcript: ["A界🙂éZ"],
		steps: [press(3, 1), drag(4, 1), release(4, 1), press(5, 1), drag(2, 1), release(2, 1), press(6, 1), drag(7, 1), release(7, 1)],
	},
	{
		name: "dock",
		transcript: ["alpha", "beta"],
		dock: ["> some input", "footer text"],
		steps: [press(3, 5), drag(5, 6), release(5, 6), ...click(4, 6), ...click(4, 6)],
	},
	{
		name: "into dock",
		transcript: numbered(6),
		steps: [press(2, 2), drag(3, 6), "tick", "tick", release(3, 6)],
	},
	{
		name: "auto-scroll",
		transcript: numbered(10),
		steps: [press(1, 3), drag(1, 1), "tick", "tick", drag(3, 2), "tick", release(3, 2)],
	},
	{
		name: "scrolled",
		transcript: numbered(10),
		steps: ["\x1b[<64;1;1M", "\x1b[<64;1;1M", press(1, 1), drag(3, 2), release(3, 2), "\x1b[<65;1;1M", "\x1b[<72;1;1M"],
	},
	{
		name: "always scrollbar",
		transcript: numbered(10).map((line) => `${line} xxxxxxxxxx`),
		scrollbar: "always",
		steps: [press(1, 1), drag(19, 3), release(19, 3)],
	},
	{
		name: "copy on select off",
		transcript: ["alpha", "beta", "gamma", "delta"],
		copyOnSelect: false,
		steps: [press(1, 1), drag(4, 2), release(4, 2)],
	},
	{
		name: "focus",
		transcript: ["alpha", "beta", "gamma", "delta"],
		steps: [
			press(1, 1),
			drag(4, 2),
			"\x1b[O",
			"\x1b[I",
			drag(5, 3),
			release(5, 3),
			press(1, 3),
			drag(3, 4),
			release(3, 4),
			"\x1b[O",
			"\x1b[I",
		],
	},
];

const out = [];
for (const testCase of cases) {
	const columns = testCase.columns ?? 20;
	const rows = testCase.rows ?? 6;
	const dock = testCase.dock ?? ["> dock", "footer"];
	const copied = [];
	const terminal = new Terminal(columns, rows);
	const tui = new TuiAltScreen(terminal, false, undefined, {
		copyOnSelect: testCase.copyOnSelect ?? true,
		// The coding agent's label, unstyled.
		scrollToEndIndicator: () => " ↓ Jump to latest message · End ",
		copySelection: async (text) => {
			copied.push(text);
			return true;
		},
	});
	const transcript = new ScrollView(lines(testCase.transcript), {
		follow: "end",
		primary: true,
		overscroll: "chain",
		scrollbar: testCase.scrollbar ?? "hidden",
	});
	tui.setLayoutRoot(
		new VStack([
			{ component: transcript, basis: 0, grow: 1, shrink: 1, minSize: 1 },
			{ component: lines(dock), basis: "auto", grow: 0, shrink: 1, minSize: 1 },
		]),
	);
	tui.start();
	tui.renderNow();
	const steps = [];
	for (const step of testCase.steps) {
		copied.length = 0;
		if (step === "tick") {
			for (const timer of [...timers]) timer.callback();
		} else {
			terminal.onInput(step);
		}
		// A copy flashes once the clipboard answers.
		await new Promise((resolve) => setImmediate(resolve));
		tui.renderNow();
		steps.push({ input: step, screen: tui.getScreenLines(), copied: [...copied] });
	}
	tui.stop();
	timers.clear();
	out.push({
		name: testCase.name,
		columns,
		rows,
		transcript: testCase.transcript,
		dock,
		scrollbar: testCase.scrollbar ?? "hidden",
		copyOnSelect: testCase.copyOnSelect ?? true,
		steps,
	});
}

const dir = join(here, "..", "selection");
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, "selection.json"), `${JSON.stringify(out, null, "\t")}\n`);
console.log(`wrote ${out.length} cases to ${dir}`);
