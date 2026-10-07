// Records pi-tui's Editor over key sequences, for crates/yapi-tui/tests/editor.rs.
import { mkdirSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(join(here, "node_modules/@earendil-works/pi-coding-agent/package.json"));
const dist = pathToFileURL(require.resolve("@earendil-works/pi-tui")).href.replace(/index\.js$/, "");
const { Editor } = await import(`${dist}components/editor.js`);

const identity = (text) => text;
const theme = {
	borderColor: identity,
	selectList: { selectedPrefix: identity, selectedText: identity, description: identity, scrollInfo: identity, noMatch: identity },
};
const stripAnsi = (line) => line.replace(/\x1b\[[0-9;]*m/g, "").replace(/\x1b_[^\x07\x1b]*(?:\x07|\x1b\\)/g, "");

const type = (text) => [...text];
const paste = (text) => `\x1b[200~${text}\x1b[201~`;
// A step that calls `insertTextAtCursor`, as the clipboard paste key does, instead of a key.
const insert = (text) => ({ insert: text });
const LEFT = "\x1b[D";
const RIGHT = "\x1b[C";
const UP = "\x1b[A";
const DOWN = "\x1b[B";
const ENTER = "\r";
const longLines = Array.from({ length: 12 }, (_, i) => `line ${i}`).join("\n");

const cases = [
	{ name: "typing", steps: [...type("hello world"), ENTER] },
	{ name: "wrap", width: 12, steps: type("the quick brown fox jumps over the lazy dog") },
	{ name: "wrap-long-word", width: 10, steps: type("abcdefghijklmnopqrstuvwxyz and more") },
	{ name: "cjk", width: 10, steps: type("日本語のテキストを折り返す") },
	{ name: "padding", width: 20, paddingX: 2, steps: type("padded text that wraps around") },
	{ name: "word-motion", steps: [...type("foo.bar baz  qux"), "\x1bb", "\x1bb", "\x1bb", "\x1bf", "\x1b[1;5D", "\x17"] },
	{ name: "kill-yank", steps: [...type("one two three"), "\x17", "\x17", "\x01", "\x19", "\x05", "\x15", "\x19", "\x1by"] },
	{ name: "kill-lines", steps: [...type("ab"), "\x0a", ...type("cd"), "\x01", "\x15", "\x0b", "\x0b", "\x19"] },
	{ name: "undo", steps: [...type("ab cd"), "\x1f", "\x1f", ...type("xy"), "\x17", "\x1f"] },
	{ name: "delete", steps: [...type("héllo wörld"), LEFT, LEFT, "\x7f", "\x1b[3~", "\x04", "\x1bd", "\x01", "\x1b[3~"] },
	{ name: "history", history: ["first", "second line\nmore"], steps: [UP, UP, UP, DOWN, DOWN, DOWN, ...type("x"), UP] },
	{ name: "vertical", width: 30, steps: [...type("a long first line here"), "\x0a", ...type("ab"), "\x0a", ...type("another long line"), UP, UP, LEFT, DOWN, DOWN, "\x1b[5~", "\x1b[6~"] },
	{ name: "vertical-wrapped", width: 10, steps: [...type("abcdefgh ijklmnop qrstuv"), UP, UP, "\x01", DOWN, DOWN, DOWN] },
	{ name: "newlines", steps: [...type("a\\"), ENTER, ...type("b"), "\x1b[13;2u", ...type("c"), "\x1b\r", "\x0a", ENTER] },
	{ name: "small-paste", steps: [...type("ab"), paste("x\ty\r\nz\x07"), ENTER] },
	{ name: "path-paste", steps: [...type("see"), paste("/tmp/file.txt")] },
	{ name: "large-paste", steps: [...type("x"), paste(longLines), LEFT, RIGHT, "\x7f", paste(longLines), paste("y".repeat(1200)), "\x01", "\x1b[C", "\x1b[C", "\x1b[3~", ENTER] },
	{ name: "marker-renumber", steps: [paste(longLines), paste(longLines), "\x01", RIGHT, "\x7f", ENTER] },
	{ name: "jump", steps: [...type("abcabc"), "\x0a", ...type("xbz"), "\x01", "\x1d", "z", "\x1b\x1d", "c", "\x1b\x1d", "c", "\x1d", "\x1d", "\x1d", "\x01"] },
	{ name: "scroll", rows: 10, steps: [...Array.from({ length: 9 }, (_, i) => [...type(`row ${i}`), "\x0a"]).flat(), UP, UP, UP, UP, UP, UP, UP] },
	{ name: "kitty-keys", steps: ["\x1b[104u", "\x1b[105;2u", "\x1b[97:65;2u", "\x1b[32;2u", "\x1b[99;5u", "\x1b[27;2;66~"] },
	{ name: "ctrl-chars", steps: [...type("ab"), "\x03", "\x1b", "\x02", "\x06", "\x0e", "\x10"] },
	{ name: "set-text", setText: "pre\tset\r\ntext", steps: ["\x1f", "\x1f"] },
	{ name: "insert-at-cursor", steps: [...type("ab"), LEFT, insert("x\ty\r\nz"), "\x1f", insert("/tmp/a.png"), ...type("c"), "\x1f"] },
];

const results = cases.map((testCase) => {
	const rows = testCase.rows ?? 24;
	const width = testCase.width ?? 40;
	const tui = { terminal: { rows, columns: width }, requestRender() {} };
	const editor = new Editor(tui, theme, { paddingX: testCase.paddingX ?? 0 });
	editor.focused = false;
	const submitted = [];
	editor.onSubmit = (text) => submitted.push(text);
	for (const entry of testCase.history ?? []) editor.addToHistory(entry);
	if (testCase.setText !== undefined) editor.setText(testCase.setText);
	editor.render(width);
	const states = testCase.steps.map((key) => {
		if (typeof key === "string") editor.handleInput(key);
		else editor.insertTextAtCursor(key.insert);
		const render = editor.render(width).map(stripAnsi);
		const cursor = editor.getCursor();
		return { key, text: editor.getText(), cursor: [cursor.line, cursor.col], render, submitted: [...submitted] };
	});
	return { name: testCase.name, width, rows, paddingX: testCase.paddingX ?? 0, history: testCase.history ?? [], setText: testCase.setText, states };
});

const out = join(here, "..", "editor");
mkdirSync(out, { recursive: true });
writeFileSync(join(out, "editor.json"), `${JSON.stringify(results, null, "\t")}\n`);
console.log(`${results.length} editor cases`);
