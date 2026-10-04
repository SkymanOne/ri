// Records pi-tui's key decoding over a corpus of terminal input, for
// crates/ri-tui/tests/keys.rs.
import { writeFileSync, mkdirSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(join(here, "node_modules/@earendil-works/pi-coding-agent/package.json"));
const dist = pathToFileURL(require.resolve("@earendil-works/pi-tui")).href.replace(/index\.js$/, "");
const keys = await import(`${dist}keys.js`);
const { StdinBuffer } = await import(`${dist}stdin-buffer.js`);

const corpus = new Set();
for (let b = 0; b < 128; b++) {
	corpus.add(String.fromCharCode(b));
	corpus.add(`\x1b${String.fromCharCode(b)}`);
}
for (const seq of [
	"\x1b[A", "\x1b[B", "\x1b[C", "\x1b[D", "\x1b[E", "\x1b[F", "\x1b[H", "\x1b[Z",
	"\x1bOA", "\x1bOB", "\x1bOC", "\x1bOD", "\x1bOE", "\x1bOF", "\x1bOH", "\x1bOM",
	"\x1bOP", "\x1bOQ", "\x1bOR", "\x1bOS", "\x1bOa", "\x1bOb", "\x1bOc", "\x1bOd", "\x1bOe",
	"\x1b[a", "\x1b[b", "\x1b[c", "\x1b[d", "\x1b[e",
	"\x1b[[A", "\x1b[[B", "\x1b[[C", "\x1b[[D", "\x1b[[E", "\x1b[[5~", "\x1b[[6~",
	"\x1b\x1b", "\x1b\r", "\x1b ", "\x1b\x7f", "\x1b\x08",
	"é", "日", "ab", "\x1b[200~pasted:3u\x1b[201~", "\x1b[", "\x1b[1;", "\x1b[;5u", "\x1b[97;u",
]) {
	corpus.add(seq);
}
for (const n of [1, 2, 3, 4, 5, 6, 7, 8, 11, 12, 13, 14, 15, 17, 18, 19, 20, 21, 23, 24]) {
	for (const tail of ["~", "$", "^"]) corpus.add(`\x1b[${n}${tail}`);
}
const modifiers = [1, 2, 3, 4, 5, 6, 7, 8, 9, 13, 17, 65, 129, 197];
const events = ["", ":1", ":2", ":3"];
for (const m of modifiers) {
	for (const e of events) {
		for (const final of "ABCDHF") corpus.add(`\x1b[1;${m}${e}${final}`);
		for (const n of [2, 3, 5, 6, 7, 8, 15]) corpus.add(`\x1b[${n};${m}${e}~`);
	}
}
const codepoints = [
	8, 9, 13, 27, 32, 45, 47, 49, 59, 61, 65, 67, 90, 91, 92, 93, 95, 96, 97, 99, 106, 122, 126, 127,
	233, 1089, 57399, 57408, 57409, 57414, 57415, 57417, 57420, 57424, 57426, 57427, 57441,
];
for (const cp of codepoints) {
	for (const m of modifiers) {
		corpus.add(`\x1b[27;${m};${cp}~`);
		for (const e of events) {
			corpus.add(m === 1 && e === "" ? `\x1b[${cp}u` : `\x1b[${cp};${m}${e}u`);
		}
	}
}
for (const seq of [
	"\x1b[97:65;2u", "\x1b[97:;2u", "\x1b[1089::99;5u", "\x1b[1089::99u", "\x1b[1089::97;5u",
	"\x1b[1074::100;5u", "\x1b[97::97;5u", "\x1b[59:58;2u", "\x1b[49:33;2u", "\x1b[57399:;2u",
	"\x1b[1081::113;3u", "\x1b[1089::99;5:3u",
]) {
	corpus.add(seq);
}

const bases = [
	"escape", "esc", "space", "tab", "enter", "return", "backspace", "insert", "delete", "clear",
	"home", "end", "pageUp", "pageDown", "up", "down", "left", "right",
	"f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12",
	..."abcdefghijklmnopqrstuvwxyz", ..."0123456789", ..."`-=[]\\;',./!@#$%^&*()_+|~{}:<>?",
	"A", "unknown", "",
];
const prefixes = ["", "shift+", "ctrl+", "alt+", "super+", "ctrl+shift+", "shift+ctrl+", "ctrl+alt+", "alt+shift+", "ctrl+super+", "ctrl+shift+alt+", "Ctrl+"];
const ids = prefixes.flatMap((prefix) => bases.map((base) => prefix + base));

function record() {
	return [...corpus].map((data) => {
		const entry = { data, matches: ids.filter((id) => keys.matchesKey(data, id)) };
		const parsed = keys.parseKey(data);
		if (parsed !== undefined) entry.parse = parsed;
		const printable = keys.decodePrintableKey(data);
		if (printable !== undefined) entry.printable = printable;
		if (keys.isKeyRelease(data)) entry.release = true;
		if (keys.isKeyRepeat(data)) entry.repeat = true;
		return entry;
	});
}

const modes = {};
for (const [name, kitty, windows] of [["legacy", false, false], ["kitty", true, false], ["windowsTerminal", false, true]]) {
	keys.setKittyProtocolActive(kitty);
	for (const variable of ["WT_SESSION", "SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]) delete process.env[variable];
	if (windows) process.env.WT_SESSION = "1";
	modes[name] = record();
}
// Other modes keep only the inputs they decode differently.
for (const name of ["kitty", "windowsTerminal"]) {
	modes[name] = modes[name].filter((entry, i) => JSON.stringify(entry) !== JSON.stringify(modes.legacy[i]));
}

// Input chunks as they arrive from stdin; "<flush>" stands for the sequence timeout.
const chunkCases = [
	["abc"], ["\x1b[A\x1b[B"], ["\x1b", "[A"], ["\x1b", "<flush>"], ["\x1b", "<flush>", "x"],
	["\x1b[<35", ";20;5m"], ["\x1b[<35;20;5M"], ["\x1b[<35;20m"], ["\x1b[<35;20m", "<flush>"],
	["\x1b[M ab"], ["\x1b[M a", "b"], ["\x1b]11;rgb:0/0/0\x07x"], ["\x1b]0;t\x1b\\y"],
	["\x1bP>|kitty\x1b\\"], ["\x1b_Gi=1;OK\x1b\\"], ["\x1bO", "P"], ["\x1bOP"],
	["\x1bx\x1b\x7f\x1b\r"], ["\x1b\x1b"], ["\x1b\x1b[27;1:3u"], ["\x1b\x1bO"], ["\x1b\x1bx"],
	["\x1b[[5~"], ["\x1b[97ua"], ["\x1b[97u", "a"], ["\x1b[97ub"], ["\x1b[97;2ua"], ["\x1b[97:65ua"],
	["\x1b[13u\r"], ["\x1b[233ué"], ["日本"],
	["\x1b[200~hello\x1b[201~"], ["a\x1b[200~multi\nline", " more\x1b[201~b"],
	["\x1b[200~x\x1b[201~\x1b[A"], ["\x1b[2", "00~paste\x1b[201~"], ["\x1b[<1\x1b[200~p\x1b[201~"],
	["\x1b[200~\x1b[201~"], ["\x1b[97u\x1b[200~p\x1b[201~a"], ["\x1b[1;5", "A"], ["\x1b[", "<flush>"],
	["\x1b[?1u\x1b[?62;22c"], ["\x1b[?", "1u"],
];
const chunks = chunkCases.map((chunks) => {
	const buffer = new StdinBuffer({ timeout: 1e9, escapeTimeout: 1e9 });
	const events = [];
	buffer.on("data", (data) => events.push({ key: data }));
	buffer.on("paste", (paste) => events.push({ paste }));
	for (const chunk of chunks) {
		if (chunk === "<flush>") {
			for (const sequence of buffer.flush()) buffer.emitDataSequence(sequence);
		} else {
			buffer.process(chunk);
		}
	}
	const result = { chunks, events, buffered: buffer.getBuffer() };
	buffer.destroy();
	return result;
});

const out = join(here, "..", "keys");
mkdirSync(out, { recursive: true });
writeFileSync(join(out, "keys.json"), `${JSON.stringify({ ids, modes })}\n`);
writeFileSync(join(out, "input.json"), `${JSON.stringify(chunks, null, "\t")}\n`);
console.log(`${corpus.size} inputs, ${ids.length} key ids, ${chunks.length} chunk cases`);
