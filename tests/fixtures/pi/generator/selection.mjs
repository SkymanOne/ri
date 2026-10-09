// Records pi-tui's fullscreen mouse handling, for crates/yapi-tui/tests/selection.rs:
// the screen lines after each mouse report or key sent to `TuiAltScreen`, the text it
// copies, the links it opens and what a docked list or component reports. The layout is pi's chat
// viewport: a scrolling transcript above a dock of lines, a focused editor with slash command
// completion, a focused `SelectList`, searchable `SettingsList` or `Input` as pi's selectors hold
// them, or a component that takes clicks, as an extension's widget does, and reports the presses
// and clicks it gets.
// Also records the rows `WheelScrollAccelerator` scrolls for timed wheel events.
import { mkdirSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(join(here, "node_modules/@earendil-works/pi-coding-agent/package.json"));
const index = pathToFileURL(require.resolve("@earendil-works/pi-tui")).href;
const {
	CombinedAutocompleteProvider,
	Editor,
	Input,
	ScrollView,
	SelectList,
	SettingsList,
	TuiAltScreen,
	VStack,
	hyperlink,
	setCapabilities,
} = await import(index);
const { WheelScrollAccelerator } = await import(index.replace(/index\.js$/, "wheel-scroll.js"));
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
const move = (x, y) => `\x1b[<35;${x};${y}M`;
const wheelUp = (x, y) => `\x1b[<64;${x};${y}M`;
const wheelDown = (x, y) => `\x1b[<65;${x};${y}M`;
const identity = (text) => text;
const editorTheme = {
	borderColor: identity,
	selectList: { selectedPrefix: identity, selectedText: identity, description: identity, scrollInfo: identity, noMatch: identity },
};
const commands = ["clear", "compact", "copy", "model"].map((name) => ({ name, description: `The ${name} command` }));
const settingsTheme = { label: identity, value: identity, description: identity, cursor: "→ ", hint: identity };
const settings = ["alpha", "beta", "gamma", "delta"].map((id) => ({
	id,
	label: id,
	description: `About ${id}`,
	currentValue: "on",
	values: ["on", "off"],
}));
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
	{
		name: "jump to latest",
		columns: 40,
		transcript: numbered(10),
		steps: [wheelUp(1, 1), wheelUp(1, 1), ...click(3, 4), ...click(10, 4), wheelUp(1, 1), ...click(36, 4)],
	},
	{
		name: "scrollbar drag",
		transcript: numbered(20),
		scrollbar: "always",
		steps: [press(20, 1), drag(20, 3), drag(20, 4), release(20, 4), press(20, 4), drag(10, 1), release(10, 1), move(5, 2)],
	},
	{
		name: "scrollbar hover",
		transcript: numbered(20),
		scrollbar: "auto",
		steps: [move(20, 2), move(19, 2), move(20, 3), press(20, 1), release(20, 1), wheelUp(5, 2), move(20, 4)],
	},
	{
		name: "hidden scrollbar",
		transcript: numbered(10),
		steps: [move(20, 1), press(20, 1), drag(18, 2), release(18, 2)],
	},
	{
		name: "editor clicks",
		rows: 8,
		transcript: ["alpha", "beta"],
		editor: { text: "hello world\nsecond line" },
		steps: [
			...click(8, 6),
			...click(20, 6),
			...click(3, 7),
			...click(5, 5),
			...click(10, 8),
			press(2, 6),
			drag(6, 7),
			release(6, 6),
			...click(1, 7),
			...click(1, 7),
			...click(3, 6),
			...click(3, 6),
			wheelUp(3, 6),
		],
	},
	{
		name: "editor wrapping",
		rows: 8,
		columns: 12,
		transcript: ["alpha"],
		editor: { text: "abcdefgh ijklmnop", paddingX: 1 },
		steps: [...click(12, 6), ...click(1, 7), ...click(4, 7), ...click(12, 7), ...click(5, 6)],
	},
	{
		name: "editor scrolled",
		rows: 12,
		transcript: ["alpha"],
		editor: { text: numbered(8).join("\n") },
		steps: [...click(3, 7), ...click(3, 11)],
	},
	{
		name: "links",
		columns: 30,
		transcript: [`see ${hyperlink("the docs", "https://example.com/docs")} now`, `${hyperlink("notes.md", "file:///work/notes.md")} here`],
		steps: [...click(7, 1), ...click(2, 1), press(6, 1), drag(10, 1), release(10, 1), ...click(3, 2), ...click(3, 2)],
	},
	{
		name: "completion",
		rows: 10,
		columns: 40,
		transcript: ["alpha"],
		editor: { text: "", commands },
		steps: ["/", wheelDown(3, 8), wheelDown(3, 8), wheelUp(3, 8), press(3, 10), drag(3, 9), release(3, 9), press(3, 9), release(3, 9)],
	},
	{
		name: "select list",
		rows: 8,
		transcript: ["alpha"],
		select: { items: ["one", "two", "three", "four", "five", "six"], maxVisible: 4 },
		steps: [
			...click(3, 6),
			wheelDown(3, 5),
			wheelUp(3, 8),
			press(3, 5),
			drag(3, 7),
			release(3, 7),
			...click(3, 8),
			wheelUp(3, 1),
			press(3, 4),
			release(3, 4),
		],
	},
	{
		name: "settings list",
		rows: 13,
		columns: 40,
		transcript: ["alpha"],
		settings: { maxVisible: 3 },
		steps: [
			"a",
			...click(3, 4),
			"b",
			"\x7f",
			...click(1, 4),
			...click(3, 5),
			wheelDown(3, 11),
			...click(3, 6),
			press(3, 7),
			drag(3, 6),
			release(3, 6),
			wheelDown(3, 4),
			wheelUp(3, 13),
		],
	},
	{
		name: "input",
		rows: 4,
		transcript: ["alpha"],
		input: "the quick brown fox jumps",
		steps: [...click(8, 4), "x", "\x05", ...click(1, 4), ...click(6, 4), "y", ...click(19, 4), wheelUp(3, 4)],
	},
	{
		// A double click inside a word selects it instead of clicking a component that takes no presses.
		name: "component clicks",
		transcript: ["alpha"],
		component: { lines: ["  agent-one done", "footer"], takesPress: false },
		steps: [...click(5, 5), ...click(5, 5), ...click(1, 5), ...click(1, 5), ...click(1, 5)],
	},
	{
		name: "component presses",
		transcript: ["alpha"],
		component: { lines: ["  agent-one done", "footer"], takesPress: true },
		steps: [
			...click(5, 5),
			...click(5, 5),
			...click(5, 5),
			...click(5, 5),
			...click(9, 5),
			press(9, 5),
			drag(10, 5),
			release(10, 5),
			...click(10, 5),
		],
	},
];

/** The component a case docks, focused, and the events its callbacks report. */
function component(testCase, tui, events) {
	if (testCase.select) {
		const items = testCase.select.items.map((value) => ({ value, label: value }));
		const list = new SelectList(items, testCase.select.maxVisible, editorTheme.selectList);
		list.onSelect = (item) => events.push(`select ${item.value}`);
		list.onSelectionChange = (item) => events.push(`move ${item.value}`);
		return list;
	}
	if (testCase.settings) {
		const onChange = (id, value) => events.push(`change ${id} ${value}`);
		return new SettingsList(structuredClone(settings), testCase.settings.maxVisible, settingsTheme, onChange, () => {}, {
			enableSearch: true,
		});
	}
	if (testCase.component) {
		const { lines: rows, takesPress } = testCase.component;
		return {
			render: () => [...rows],
			invalidate() {},
			handleMouse(event) {
				if (event.type === "press") events.push("press");
				if (event.type === "click") events.push(`click ${event.clickCount}`);
				return event.type === "click" || (takesPress && event.type === "press") ? { handled: true } : undefined;
			},
		};
	}
	if (testCase.input !== undefined) {
		const input = new Input();
		input.setValue(testCase.input);
		return input;
	}
	if (!testCase.editor) return undefined;
	const editor = new Editor(tui, editorTheme, { paddingX: testCase.editor.paddingX ?? 0 });
	editor.setText(testCase.editor.text);
	if (testCase.editor.commands) editor.setAutocompleteProvider(new CombinedAutocompleteProvider(testCase.editor.commands, here));
	return editor;
}

const out = [];
for (const testCase of cases) {
	const columns = testCase.columns ?? 20;
	const rows = testCase.rows ?? 6;
	const dock = testCase.dock ?? ["> dock", "footer"];
	const copied = [];
	const opened = [];
	const terminal = new Terminal(columns, rows);
	const tui = new TuiAltScreen(terminal, false, undefined, {
		copyOnSelect: testCase.copyOnSelect ?? true,
		// The coding agent's label, unstyled.
		scrollToEndIndicator: () => " ↓ Jump to latest message · End ",
		copySelection: async (text) => {
			copied.push(text);
			return true;
		},
		openUrl: (url) => opened.push(url),
	});
	const events = [];
	const docked = component(testCase, tui, events);
	if (docked) tui.setFocus(docked);
	const transcript = new ScrollView(lines(testCase.transcript), {
		follow: "end",
		primary: true,
		overscroll: "chain",
		scrollbar: testCase.scrollbar ?? "hidden",
	});
	tui.setLayoutRoot(
		new VStack([
			{ component: transcript, basis: 0, grow: 1, shrink: 1, minSize: 1 },
			{ component: docked ?? lines(dock), basis: "auto", grow: 0, shrink: 1, minSize: 1 },
		]),
	);
	tui.start();
	tui.renderNow();
	const steps = [];
	for (const step of testCase.steps) {
		copied.length = 0;
		opened.length = 0;
		events.length = 0;
		if (step === "tick") {
			for (const timer of [...timers]) timer.callback();
		} else {
			terminal.onInput(step);
		}
		// A copy flashes once the clipboard answers.
		await new Promise((resolve) => setImmediate(resolve));
		tui.renderNow();
		steps.push({
			input: step,
			screen: tui.getScreenLines(),
			copied: [...copied],
			...(opened.length ? { opened: [...opened] } : {}),
			...(events.length ? { events: [...events] } : {}),
		});
	}
	tui.stop();
	timers.clear();
	out.push({
		name: testCase.name,
		columns,
		rows,
		transcript: testCase.transcript,
		dock,
		editor: testCase.editor ?? null,
		...(testCase.select ? { select: testCase.select } : {}),
		...(testCase.settings ? { settings: { ...testCase.settings, items: settings } } : {}),
		...(testCase.input !== undefined ? { input: testCase.input } : {}),
		...(testCase.component ? { component: testCase.component } : {}),
		scrollbar: testCase.scrollbar ?? "hidden",
		copyOnSelect: testCase.copyOnSelect ?? true,
		steps,
	});
}

// Wheel events as [direction, milliseconds].
const spin = (gap, count, direction = 1, start = 0) =>
	Array.from({ length: count }, (_, index) => [direction, start + index * gap]);
const wheelCases = [
	{ name: "fixed lines", lines: 3, events: spin(10, 3) },
	{ name: "not accelerated", accelerate: false, events: spin(20, 4) },
	{ name: "isolated notches", events: spin(300, 3) },
	{ name: "100 ms apart", events: spin(100, 4) },
	{ name: "50 ms apart", events: spin(50, 6) },
	{ name: "20 ms apart", events: spin(20, 8) },
	{ name: "capped", events: spin(10, 6) },
	{ name: "one notch in bursts", events: spin(4, 5) },
	{ name: "fractions carry", events: spin(40, 6) },
	{ name: "slowing down", events: [0, 10, 30, 70, 150, 300].map((time) => [1, time]) },
	{ name: "direction change", events: [...spin(20, 3), ...spin(20, 3, -1, 60)] },
	{ name: "pause ends a spin", events: [...spin(20, 3), ...spin(20, 3, 1, 241)] },
];
const wheel = wheelCases.map(({ name, lines = "auto", accelerate = true, events }) => {
	const accelerator = new WheelScrollAccelerator(lines, accelerate);
	return { name, lines, accelerate, events, steps: events.map(([direction, time]) => accelerator.next(direction, time)) };
});

// Clicks on chat items, for crates/yapi/src/interactive/mod.rs: pi's message components in the chat
// viewport as interactive mode adds them, with tool output collapsed. Each step clicks the first cell
// showing some text, or the cell `dy` rows below it and at column `x` when given.
const pi = await import("@earendil-works/pi-coding-agent");
const { Container, Spacer, setKeybindings, visibleWidth } = await import(index);
pi.initTheme("dark", false);
const agentDist = import.meta.resolve("@earendil-works/pi-coding-agent").replace(/index\.js$/, "");
const { KeybindingsManager } = await import(`${agentDist}core/keybindings.js`);
setKeybindings(new KeybindingsManager());
const zero = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 };
const expand = {
	columns: 60,
	rows: 48,
	cwd: "/work",
	skill: '<skill name="review" location="/skills/review/SKILL.md">\nRead the diff.\n\nReport each finding.\n</skill>\n\nCheck this branch',
	assistant: {
		role: "assistant",
		content: [
			{ type: "thinking", thinking: "First I plan the review." },
			{ type: "text", text: "Reading the notes." },
		],
		api: "anthropic-messages",
		provider: "anthropic",
		model: "claude-sonnet-4-5",
		usage: { ...zero, totalTokens: 0, cost: zero },
		stopReason: "toolUse",
		timestamp: 0,
	},
	reads: [
		{ path: "notes.txt", output: numbered(14).join("\n") },
		{ path: "todo.txt", output: null },
	],
	compaction: { tokensBefore: 12345, summary: "The user asked for a review." },
	branch: { summary: "Tried another approach." },
	clicks: [
		{ text: "[skill]", dy: -1 },
		{ text: "[skill]" },
		{ text: "First I plan" },
		{ text: "Thinking..." },
		{ text: "read notes.txt" },
		{ text: "line 3" },
		{ text: "read notes.txt", x: 59 },
		{ text: "read todo.txt" },
		{ text: "Reading the notes." },
		{ text: "[compaction]" },
		{ text: "[branch]" },
		{ text: "[branch]" },
	],
};
{
	const terminal = new Terminal(expand.columns, expand.rows);
	const tui = new TuiAltScreen(terminal, false);
	const chat = new Container();
	const block = pi.parseSkillBlock(expand.skill);
	chat.addChild(new pi.SkillInvocationMessageComponent(block));
	chat.addChild(new Spacer(1));
	chat.addChild(new pi.UserMessageComponent(block.userMessage));
	chat.addChild(new pi.AssistantMessageComponent(expand.assistant));
	for (const [index, read] of expand.reads.entries()) {
		const definition = pi.createReadToolDefinition(expand.cwd);
		const tool = new pi.ToolExecutionComponent("read", `read-${index}`, { path: read.path }, {}, definition, tui, expand.cwd);
		if (read.output) tool.updateResult({ content: [{ type: "text", text: read.output }], isError: false });
		chat.addChild(tool);
	}
	chat.addChild(new Spacer(1));
	chat.addChild(new pi.CompactionSummaryMessageComponent({ role: "compactionSummary", ...expand.compaction, timestamp: 0 }));
	chat.addChild(new Spacer(1));
	chat.addChild(new pi.BranchSummaryMessageComponent({ role: "branchSummary", ...expand.branch, fromId: "a", timestamp: 0 }));
	const transcript = new ScrollView(chat, { follow: "end", primary: true, overscroll: "chain", scrollbar: "hidden" });
	tui.setLayoutRoot(
		new VStack([
			{ component: transcript, basis: 0, grow: 1, shrink: 1, minSize: 1 },
			{ component: lines(["> dock", "footer"]), basis: "auto", grow: 0, shrink: 1, minSize: 1 },
		]),
	);
	tui.start();
	tui.renderNow();
	// Escape sequences, so text is found by the columns it shows in.
	const plain = (line) => line.replace(/\x1b\[[0-9;:]*[A-Za-z]|\x1b\][^\x07]*\x07/g, "");
	expand.first = tui.getScreenLines();
	expand.steps = [];
	for (const { text, dy = 0, x } of expand.clicks) {
		const rows = tui.getScreenLines().map(plain);
		const row = rows.findIndex((line) => line.includes(text));
		if (row < 0) throw new Error(`${text} is not on the screen`);
		const column = x ?? visibleWidth(rows[row].slice(0, rows[row].indexOf(text)));
		const input = click(column + 1, row + dy + 1);
		for (const report of input) terminal.onInput(report);
		tui.renderNow();
		expand.steps.push({ input, screen: tui.getScreenLines() });
	}
	tui.stop();
}

const dir = join(here, "..", "selection");
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, "selection.json"), `${JSON.stringify(out, null, "\t")}\n`);
writeFileSync(join(dir, "wheel.json"), `${JSON.stringify(wheel, null, "\t")}\n`);
writeFileSync(join(dir, "expand.json"), `${JSON.stringify(expand, null, "\t")}\n`);
console.log(`wrote ${out.length} selection, ${wheel.length} wheel and ${expand.steps.length} expand cases to ${dir}`);
