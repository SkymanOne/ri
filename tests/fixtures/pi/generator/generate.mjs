// Generates the pi golden fixtures in ../ with the published pi package.
// Usage: npm install --ignore-scripts && node generate.mjs   (Node >= 22.19)
// No network access is needed after the install. See ../README.md.

import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, readFileSync, readdirSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const fixtures = join(here, "..");
const piDir = join(here, "node_modules/@earendil-works/pi-coding-agent");
const legacySource = process.env.PI_SOURCE; // optional: pi checkout providing test/fixtures/*.jsonl

// A fixed root keeps paths inside the fixtures stable across runs.
const root = "/tmp/yapi-pi-fixtures";
const home = join(root, "home");
const agentDir = join(root, "agent");
const cwd = join(root, "project");
rmSync(root, { recursive: true, force: true });
for (const dir of [home, agentDir, cwd]) mkdirSync(dir, { recursive: true });
// The system prompt names pi's docs in its package directory: a link keeps the checkout's path out.
const packageDir = join(root, "pi-coding-agent");
symlinkSync(piDir, packageDir);
Object.assign(process.env, {
	HOME: home,
	PI_CODING_AGENT_DIR: agentDir,
	PI_PACKAGE_DIR: packageDir,
	PI_OFFLINE: "1",
	PI_SKIP_VERSION_CHECK: "1",
});

const PNG_1X1 =
	"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

function cli(...args) {
	const result = spawnSync(process.execPath, [join(piDir, "dist/bundle/cli.js"), ...args], {
		cwd,
		env: process.env,
		encoding: "utf8",
	});
	if (result.status !== 0) throw new Error(`pi ${args.join(" ")} failed:\n${result.stderr}${result.stdout}`);
}

function save(source, target) {
	const path = join(fixtures, target);
	mkdirSync(dirname(path), { recursive: true });
	copyFileSync(source, path);
}

function writeJson(path, value) {
	mkdirSync(dirname(path), { recursive: true });
	writeFileSync(path, JSON.stringify(value, null, 2));
}

// --- Config files written by the pi CLI -------------------------------------

// Legacy keybinding names make pi's startup migration rewrite the file.
writeJson(join(agentDir, "keybindings.json"), {
	cursorUp: ["up", "ctrl+p"],
	interrupt: "escape",
	"tui.editor.cursorDown": "ctrl+n",
});
cli("--offline", "--help");
cli("mcp", "add", "files", "--", "npx", "-y", "@modelcontextprotocol/server-filesystem", ".");
cli("mcp", "add", "docs", "--url", "https://example.com/mcp", "--header", "X-Team=yapi", "--exposure", "direct");
cli("mcp", "add", "local", "-l", "--", "node", "server.js", "--verbose");

// --- SDK imports (after env setup, so pi reads the temp agent dir) ----------

const pi = await import("@earendil-works/pi-coding-agent");
const ai = await import(join(piDir, "node_modules/@earendil-works/pi-ai/dist/index.js"));

// --- Settings, trust, auth ---------------------------------------------------

new pi.ProjectTrustStore(agentDir).set(cwd, true);
const settings = pi.SettingsManager.create(cwd, agentDir, { projectTrusted: true });
settings.setDefaultModelAndProvider("anthropic", "claude-sonnet-4-5");
settings.setDefaultThinkingLevel("medium");
settings.setModelThinkingLevel("openai", "gpt-5", "high");
settings.setTheme("dark");
settings.setTransport("sse");
settings.setSteeringMode("all");
settings.setCompactionEnabled(true);
settings.setHttpIdleTimeoutMs(120000);
settings.setCacheWarmingMode("off");
settings.setQuietStartup("header");
settings.setNpmCommand(["npm"]);
settings.setPackages(["npm:pi-example@1.2.3", { source: "git:github.com/example/pi-tools@v1", skills: ["!legacy/**"] }]);
settings.setEnabledModels(["anthropic/*", "openai/gpt-5"]);
settings.setFullscreenWheelScrollLines("auto");
settings.setEditorPaddingX(1);
settings.setWarnings({ anthropicExtraUsage: false });
settings.setProjectPackages(["./local-extension"]);
settings.setProjectSkillPaths(["skills"]);
await settings.flush();

const runtime = await pi.ModelRuntime.create({
	authPath: join(agentDir, "auth.json"),
	modelsPath: null,
	allowModelNetwork: false,
});

// Providers whose logins return fixed credentials, so pi's AuthStorage writes them.
const authProvider = (id, auth) =>
	ai.createProvider({
		id,
		auth,
		models: [{ id: "fixture-1", name: "Fixture", api: "faux", provider: id, baseUrl: "http://localhost:0" }],
		api: { stream: () => undefined, streamSimple: () => undefined },
	});
runtime.registerNativeProvider(
	authProvider("fixture-key", {
		apiKey: {
			name: "Fixture API key",
			login: async () => ({ type: "api_key", key: "!cat ~/.fixture-key", env: { FIXTURE_REGION: "eu" } }),
			resolve: async () => undefined,
		},
	}),
);
runtime.registerNativeProvider(
	authProvider("fixture-oauth", {
		oauth: {
			name: "Fixture subscription",
			login: async () => ({ type: "oauth", access: "access-token", refresh: "refresh-token", expires: 1893456000000, accountId: "acct-1" }),
			refresh: async (credential) => credential,
			toAuth: async () => ({}),
		},
	}),
);
const interaction = { prompt: async () => "sk-fixture-openai", notify() {} };
await runtime.login("openai", "api_key", interaction);
await runtime.login("fixture-key", "api_key", interaction);
await runtime.login("fixture-oauth", "oauth", interaction);

// --- Hand-authored files that pi reads but never writes -----------------------

const allSettings = {
	defaultProvider: "anthropic",
	defaultModel: "claude-sonnet-4-5",
	defaultThinkingLevel: "high",
	modelThinkingLevels: { "openai/gpt-5": "xhigh" },
	enabledModels: ["anthropic/*"],
	thinkingBudgets: { minimal: 1024, low: 2048, medium: 8192, high: 16384 },
	transport: "websocket-cached",
	steeringMode: "one-at-a-time",
	followUpMode: "all",
	compaction: {
		enabled: true,
		reserveTokens: 16384,
		keepRecentTokens: 20000,
		modelOverrides: { "anthropic/claude-haiku-4-5": { reserveTokens: 8192, keepRecentTokens: 10000 } },
	},
	branchSummary: { reserveTokens: 16384, skipPrompt: true },
	retry: {
		enabled: true,
		maxRetries: 3,
		baseDelayMs: 2000,
		maxAgentDelayMs: 60000,
		provider: { timeoutMs: 600000, maxRetries: 2, maxRetryDelayMs: 60000 },
	},
	defaultTools: ["+grep", "-write"],
	doubleEscapeAction: "fork",
	treeFilterMode: "no-tools",
	codemode: { mode: "on", inlineBudget: 3000 },
	cacheWarming: "idle",
	defaultProjectTrust: "always",
	theme: "light",
	hideThinkingBlock: false,
	showCacheMissNotices: true,
	collapseChangelog: true,
	showHardwareCursor: false,
	quietStartup: true,
	editorPaddingX: 2,
	outputPad: 1,
	autocompleteMaxVisible: 8,
	terminal: {
		showImages: true,
		imageWidthCells: 60,
		clearOnShrink: false,
		showTerminalProgress: true,
		hyperlinks: "auto",
		images: "kitty",
		trueColor: "auto",
	},
	images: { autoResize: true, blockImages: false },
	markdown: { codeBlockIndent: "  ", mermaid: "final" },
	warnings: { anthropicExtraUsage: true },
	tuiMode: "fullscreen",
	fullscreenExitOutput: "resume-hint",
	fullscreenScrollbar: "auto",
	fullscreenCopyOnSelect: true,
	fullscreenWheelScrollLines: 3,
	externalEditor: "vim",
	shellPath: "/bin/zsh",
	shellCommandPrefix: "shopt -s expand_aliases",
	sessionDir: "~/sessions",
	httpProxy: "http://proxy.local:8080",
	npmCommand: ["mise", "exec", "node@22", "--", "npm"],
	httpIdleTimeoutMs: "disabled",
	websocketConnectTimeoutMs: 15000,
	packages: [
		"npm:@scope/pi-pack",
		{ source: "./vendor/pack", autoload: false, extensions: ["+ext/main.ts"], skills: [], prompts: ["!draft-*"], themes: ["dark.json"] },
	],
	extensions: ["-builtin:mcp", "~/ext/tool.ts"],
	skills: ["~/skills"],
	prompts: ["prompts"],
	themes: ["themes"],
	enableSkillCommands: false,
	lastChangelogVersion: "1.0.0",
	trackingId: "00000000-0000-0000-0000-000000000000",
	enableAnalytics: false,
	enableInstallTelemetry: false,
	deviceId: "11111111-1111-1111-1111-111111111111",
};
const allSettingsDir = join(root, "agent-all");
writeJson(join(allSettingsDir, "settings.json"), allSettings);
const allSettingsManager = pi.SettingsManager.create(cwd, allSettingsDir, { projectTrusted: false });
if (allSettingsManager.drainErrors?.().length) throw new Error("settings-all-fields.json rejected by pi");
allSettingsManager.getCompactionSettings();

const models = {
	providers: {
		"local-llm": {
			name: "Local LLM",
			baseUrl: "http://localhost:11434/v1",
			api: "openai-completions",
			apiKey: "$LOCAL_LLM_KEY",
			headers: { "X-Client": "!echo yapi" },
			compat: { supportsDeveloperRole: false, maxTokensField: "max_tokens" },
			authHeader: true,
			models: [
				{
					id: "qwen3-coder",
					name: "Qwen3 Coder",
					reasoning: true,
					thinkingLevelMap: { off: null, low: "low", high: "high" },
					input: ["text", "image"],
					inputLimits: {
						maxRequestBytes: 10485760,
						images: { resize: { maxWidth: 1568, maxHeight: 1568, maxBytes: 5242880, jpegQuality: 85 }, maxPerMessage: 4 },
					},
					cost: { input: 0.15, output: 0.6, cacheRead: 0.015, cacheWrite: 0, tiers: [{ inputTokensAbove: 200000, input: 0.3, output: 1.2, cacheRead: 0.03, cacheWrite: 0 }] },
					promptCache: { short: 300, long: 3600 },
					contextWindow: 262144,
					maxTokens: 32768,
					samplingParams: { temperature: 0.7, top_p: 0.8 },
					headers: { "X-Model": "qwen" },
				},
			],
		},
		anthropic: {
			modelOverrides: { "claude-sonnet-4-5": { name: "Sonnet (proxy)", contextWindow: 1000000, cost: { input: 3 } } },
			baseUrl: "https://proxy.example.com/anthropic",
		},
	},
};
const modelsPath = join(root, "models.json");
writeJson(modelsPath, models);
const modelsRuntime = await pi.ModelRuntime.create({ authPath: join(root, "models-auth.json"), modelsPath, allowModelNetwork: false });
if (modelsRuntime.getError() || !modelsRuntime.getModel("local-llm", "qwen3-coder")) {
	throw new Error(`models.json rejected by pi: ${modelsRuntime.getError()}`);
}

// --- Sessions ------------------------------------------------------------------

writeFileSync(join(cwd, "pixel.png"), Buffer.from(PNG_1X1, "base64"));
const image = { type: "image", data: PNG_1X1, mimeType: "image/png" };
const fauxModels = (ids) => ids.map((id) => ({ id, reasoning: true, input: ["text", "image"], contextWindow: 200000, maxTokens: 8192 }));
const faux = {
	anthropic: ai.fauxProvider({ provider: "anthropic", api: "anthropic-messages", models: fauxModels(["claude-sonnet-4-5", "claude-haiku-4-5"]) }),
	openai: ai.fauxProvider({ provider: "openai", api: "openai-responses", models: fauxModels(["gpt-5"]) }),
	google: ai.fauxProvider({ provider: "google", api: "google-generative-ai", models: fauxModels(["gemini-2.5-pro"]), tokensPerSecond: 20 }),
	groq: ai.fauxProvider({ provider: "groq", api: "openai-completions", models: fauxModels(["llama-4"]) }),
};
for (const handle of Object.values(faux)) runtime.registerNativeProvider(handle.provider);

const say = (content, options) => ai.fauxAssistantMessage(content, options);
const call = (name, args, id) => ai.fauxToolCall(name, args, { id });

const loader = new pi.DefaultResourceLoader({
	cwd,
	agentDir,
	settingsManager: settings,
	noSkills: true,
	noContextFiles: true,
	noPromptTemplates: true,
	noThemes: true,
	noExtensions: true,
});
await loader.reload();

const sessionManager = pi.SessionManager.create(cwd, undefined, { id: "019a0000-0000-7000-8000-000000000001" });
const { session } = await pi.createAgentSession({
	cwd,
	agentDir,
	modelRuntime: runtime,
	settingsManager: settings,
	resourceLoader: loader,
	sessionManager,
	model: runtime.getModel("anthropic", "claude-sonnet-4-5"),
	thinkingLevel: "medium",
});
await session.bindExtensions({});
// In-memory only, applied after session setup reloads settings: fast retries, tiny compaction window.
settings.applyOverrides({ retry: { baseDelayMs: 1, maxRetries: 1 }, compaction: { keepRecentTokens: 1 } });

faux.anthropic.setResponses([
	say(
		[
			{ type: "thinking", thinking: "The user greets me.", thinkingSignature: "sig-abc" },
			{ type: "thinking", thinking: "[Reasoning redacted]", thinkingSignature: "opaque", redacted: true },
			ai.fauxText("Hello! How can I help?"),
		],
		{ responseId: "msg_01" },
	),
]);
await session.prompt("Hello");

faux.anthropic.setResponses([
	say([ai.fauxText("Writing the file."), call("write", { path: "notes.txt", content: "one\ntwo\n" }, "toolu_1")], { stopReason: "toolUse" }),
	say([call("edit", { path: "notes.txt", edits: [{ oldText: "two", newText: "three" }] }, "toolu_2")], { stopReason: "toolUse" }),
	say(
		[call("bash", { command: "cat notes.txt" }, "toolu_3"), call("read", { path: "missing.txt" }, "toolu_4"), call("read", { path: "pixel.png" }, "toolu_5")],
		{ stopReason: "toolUse" },
	),
	say("Done: notes.txt now ends with three."),
]);
await session.prompt("Create notes.txt, then fix it.");

faux.anthropic.setResponses([say("A single transparent pixel.")]);
await session.prompt("What is in this image?", { images: [image] });

await session.executeBash("echo hello");
await session.executeBash("echo hidden", undefined, { excludeFromContext: true });
session.recordBashResult("sleep 100", {
	output: "partial output",
	exitCode: undefined,
	cancelled: true,
	truncated: true,
	fullOutputPath: join(root, "bash-full.log"),
});

await session.sendCustomMessage({ customType: "fixture-note", content: "Reminder: run tests.", display: true, details: { priority: 1 } });
sessionManager.appendCustomEntry("fixture-edge-values", {
	numbers: [0, -0, 0.000003, 1e-7, 1e21, 0.1 + 0.2, 2 ** 53, 123.456, 5e-324, 1.7976931348623157e308, -42],
	strings: ['quote " backslash \\ slash /', "control \u0000\u0001\u001f\b\f\n\r\t", "separators    ", "emoji 🦀 math 𝕏", "del \u007f"],
	nested: { empty: {}, list: [], null: null, flag: true },
});

session.setThinkingLevel("high");
await session.setModel(runtime.getModel("openai", "gpt-5"));
faux.openai.setResponses([
	say([
		{ type: "thinking", thinking: "Short plan.", thinkingSignature: '{"id":"rs_1","type":"reasoning"}' },
		{ type: "text", text: "Using the Responses API.", textSignature: "msg_1" },
	]),
	say("", { stopReason: "error", errorMessage: "400 invalid_request_error: unsupported parameter" }),
]);
await session.prompt("Which API is this?");
await session.prompt("Trigger an error.");

await session.setModel(runtime.getModel("groq", "llama-4"));
faux.groq.setResponses([say("", { stopReason: "error", errorMessage: "529 overloaded_error: Overloaded" }), say("Recovered after a retry.")]);
await session.prompt("Retry please.");

await session.setModel(runtime.getModel("google", "gemini-2.5-pro"));
faux.google.setResponses([say("This answer is long enough to be interrupted by an abort request. ".repeat(20))]);
const unsubscribe = session.subscribe((event) => {
	if (event.type === "message_update") {
		unsubscribe();
		void session.abort();
	}
});
await session.prompt("Write something long.");

session.setSessionName("Fixture session");
const labelled = sessionManager.getLeafId();
sessionManager.appendLabelChange(labelled, "checkpoint");
sessionManager.appendLabelChange(labelled, undefined);

// Compaction needs a context-visible last turn; the aborted message above is omitted from context.
await session.setModel(runtime.getModel("anthropic", "claude-sonnet-4-5"));
faux.anthropic.setResponses([say("We created and edited notes.txt.")]);
await session.prompt("Where are we?");
const summary = "## Goal\nKeep notes.txt up to date.\n\n## Progress\n- Created and edited notes.txt";
faux.anthropic.setResponses([say(summary), say(summary)]);
await session.compact("Focus on file changes.");

const branchPoint = sessionManager.getEntries().find((entry) => entry.type === "message" && entry.message.role === "user").id;
faux.anthropic.setResponses([say("Abandoned path: created and edited notes.txt."), say("Starting over on a new branch.")]);
await session.navigateTree(branchPoint, { summarize: true, label: "alternative" });
await session.prompt("Let us try another approach.");

// Entries pi only writes through extensions, cache warming or hooks.
const usage = (overrides) => ({
	input: 1200,
	output: 340,
	cacheRead: 8000,
	cacheWrite: 512,
	totalTokens: 10052,
	cost: { input: 0.0036, output: 0.0051, cacheRead: 0.0024000000000000002, cacheWrite: 0.00192, total: 0.01302 },
	...overrides,
});
sessionManager.appendUsage("cache_warming", "anthropic", "claude-sonnet-4-5", usage({}), "idle refresh");
const lastUser = sessionManager.getEntries().findLast((entry) => entry.type === "message" && entry.message.role === "user").id;
sessionManager.appendContextEdit(lastUser, { content: [{ type: "text", text: "Let us try another approach (edited)." }] });
sessionManager.appendCompaction("Hook summary.", lastUser, 4096, { readFiles: ["notes.txt"], modifiedFiles: [] }, true, usage({ input: 10 }));

// Provider-shaped assistant messages with the key orders pi's real adapters produce.
const timestamp = Date.now();
sessionManager.appendMessage({
	role: "assistant",
	content: [{ type: "thinking", thinking: "Check the cache.", thinkingSignature: "EqQBCkYIBxgCKkA" }, { type: "text", text: "Cached." }],
	api: "anthropic-messages",
	provider: "anthropic",
	model: "claude-sonnet-4-5",
	providerThinkingLevel: "medium",
	usage: { ...usage({}), cacheWrite1h: 256, reasoning: 120 },
	stopReason: "stop",
	timestamp,
	responseId: "msg_0123",
	responseModel: "claude-sonnet-4-5-20250929",
	rawStopReason: "end_turn",
	thinkingLevel: "medium",
});
sessionManager.appendMessage({
	role: "assistant",
	content: [ai.fauxText("Completions reply.")],
	api: "openai-completions",
	provider: "groq",
	model: "llama-4",
	usage: { input: 50, output: 7, cacheRead: 0, cacheWrite: 0, reasoning: 0, totalTokens: 57, cost: { input: (0.11 / 1e6) * 50, output: (0.34 / 1e6) * 7, cacheRead: 0, cacheWrite: 0, total: (0.11 / 1e6) * 50 + (0.34 / 1e6) * 7 } },
	stopReason: "length",
	timestamp: timestamp + 1,
	responseId: "chatcmpl-1",
	rawStopReason: "length",
	thinkingLevel: "off",
});
sessionManager.appendMessage({
	role: "assistant",
	content: [
		{ type: "thinking", thinking: "Gemini thoughts.", thinkingSignature: "CiQB0e2Kb" },
		{ type: "toolCall", id: "call_g1", name: "ls", arguments: { path: "." }, thoughtSignature: "CiQB0e2Kc" },
	],
	api: "google-generative-ai",
	provider: "google",
	model: "gemini-2.5-pro",
	usage: { input: 900, output: 60, cacheRead: 0, cacheWrite: 0, reasoning: 40, totalTokens: 960, cost: { input: 0.001125, output: 0.0006, cacheRead: 0, cacheWrite: 0, total: 0.001725 } },
	stopReason: "toolUse",
	timestamp: timestamp + 2,
	responseId: "resp-g1",
	rawStopReason: "STOP",
	thinkingLevel: "high",
});
sessionManager.appendMessage({
	role: "assistant",
	content: [{ type: "text", text: "" }],
	api: "anthropic-messages",
	provider: "anthropic",
	model: "claude-sonnet-4-5",
	usage: usage({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } }),
	stopReason: "error",
	errorMessage: "No API key for provider: anthropic",
	timestamp: timestamp + 3,
});
sessionManager.appendMessage({
	role: "system",
	content: "",
	sections: { project_context: "# AGENTS.md\nUse tabs.", skills: null },
	timestamp: timestamp + 4,
	toolsAdded: [{ name: "grep", description: "Search file contents", parameters: { type: "object", properties: { pattern: { type: "string" } }, required: ["pattern"] } }],
	toolsRemoved: [{ name: "write" }],
});

const mainFile = sessionManager.getSessionFile();
const exportFile = join(root, "export.jsonl");
session.exportToJsonl(exportFile);
session.dispose();

const branchedFile = pi.SessionManager.open(mainFile).createBranchedSession(labelled);
const forked = pi.SessionManager.forkFrom(mainFile, join(root, "other-project"));
const child = pi.SessionManager.create(cwd, undefined, { parentSession: mainFile });
child.appendMessage({ role: "user", content: [{ type: "text", text: "Continue from the parent." }], timestamp: Date.now() });
child.appendMessage(say("Continuing.", { timestamp: Date.now() }));

save(mainFile, "sessions/main.jsonl");
save(exportFile, "sessions/export.jsonl");
save(branchedFile, "sessions/branched.jsonl");
save(forked.getSessionFile(), "sessions/forked.jsonl");
save(child.getSessionFile(), "sessions/child.jsonl");

// --- Legacy v1 sessions, rewritten as v3 by pi ---------------------------------

// Excerpts of pi's own test sessions (line ranges, 1-based inclusive). They are committed
// under ../legacy; set PI_SOURCE to a pi v1.0.0 checkout to cut them again.
const legacy = {
	"before-compaction": [[1, 3], [9, 25], [27, 40], [355, 365], [637, 645], [710, 716]],
	"large-session": [[1, 6], [9, 12], [17, 27], [29, 32], [34, 60]],
};
for (const [name, ranges] of Object.entries(legacy)) {
	const excerptPath = join(fixtures, "legacy", `${name}.v1.jsonl`);
	if (legacySource) {
		const lines = readFileSync(join(legacySource, "packages/coding-agent/test/fixtures", `${name}.jsonl`), "utf8").split("\n");
		mkdirSync(dirname(excerptPath), { recursive: true });
		writeFileSync(excerptPath, `${ranges.flatMap(([from, to]) => lines.slice(from - 1, to)).join("\n")}\n`);
	}
	const migrated = join(root, `${name}.jsonl`);
	copyFileSync(excerptPath, migrated);
	pi.SessionManager.open(migrated, join(root, "legacy-sessions"));
	save(migrated, `sessions/legacy-${name}.jsonl`);
}

// --- Config files ----------------------------------------------------------------

for (const name of ["settings", "auth", "keybindings", "mcp"]) save(join(agentDir, `${name}.json`), `agent/${name}.json`);
save(join(allSettingsDir, "settings.json"), "agent/settings-all-fields.json");
save(modelsPath, "agent/models.json");
for (const name of ["settings", "mcp"]) save(join(cwd, ".pi", `${name}.json`), `project/${name}.json`);

const written = readdirSync(fixtures, { recursive: true }).filter((p) => /\.(json|jsonl)$/.test(p) && !p.startsWith("generator"));
console.log(`Wrote ${written.length} fixtures:\n  ${written.sort().join("\n  ")}`);
