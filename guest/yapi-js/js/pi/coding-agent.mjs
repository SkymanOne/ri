// `@earendil-works/pi-coding-agent` for extensions in yapi: the API helpers
// extensions use, built-in tools backed by yapi's own, and stubs for the rest of
// pi's exports so every import links. A stub throws when called.
import { Container, Editor, Text, backgroundAnsi, foregroundAnsi, parseColor } from "@earendil-works/pi-tui";

const yapi = globalThis.__yapi;

const unavailable = (name) => yapi.unsupported(`${name} from @earendil-works/pi-coding-agent is not available in yapi extensions`);

// ----- configuration ------------------------------------------------------
/** yapi keeps project files in `.yapi/` and global ones in `~/.yapi/agent`. */
export const CONFIG_DIR_NAME = ".yapi";
/** The pi extension API version yapi implements. */
export const VERSION = "1.0.0";
export const CURRENT_SESSION_VERSION = 3;
export const DEFAULT_MAX_BYTES = 50 * 1024;
export const DEFAULT_MAX_LINES = 2000;
export const DEFAULT_COMPACTION_SETTINGS = { enabled: true, reserveTokens: 16384, keepRecentTokens: 20000 };
export const VIRTUAL_MODEL_STATE_ENTRY = "pi.virtual-model-state";

export function getAgentDir() {
	return yapi.request("agentDir");
}
export function getPackageDir() {
	return getAgentDir();
}
export function getDocsPath() {
	return `${getAgentDir()}/docs`;
}
export function getExamplesPath() {
	return `${getAgentDir()}/examples`;
}
export function getReadmePath() {
	return `${getAgentDir()}/README.md`;
}
export function getShellConfig() {
	return { shell: process.env.SHELL || "/bin/sh", args: ["-c"] };
}

// ----- tool definitions and events -------------------------------------------
export function defineTool(tool) {
	return tool;
}
export function isToolCallEventType(toolName, event) {
	return event.toolName === toolName;
}
const resultOf = (name) => (event) => event.toolName === name;
export const isBashToolResult = resultOf("bash");
export const isPowerShellToolResult = resultOf("powershell");
export const isReadToolResult = resultOf("read");
export const isEditToolResult = resultOf("edit");
export const isWriteToolResult = resultOf("write");
export const isGrepToolResult = resultOf("grep");
export const isFindToolResult = resultOf("find");
export const isLsToolResult = resultOf("ls");

export const createEventBus = yapi.createEventBus;

// ----- built-in tools, run by yapi ----------------------------------------------------
function builtinTool(name, cwd, options) {
	const info = yapi.request("builtin.tool", { name, cwd: cwd ?? process.cwd() });
	return {
		name: info.name,
		label: info.label,
		description: info.description,
		promptSnippet: info.promptSnippet,
		promptGuidelines: info.promptGuidelines,
		parameters: info.parameters,
		// pi's edit tool draws its own shell; yapi draws the built-in renderers.
		...(name === "edit" ? { renderShell: "self" } : {}),
		async execute(toolCallId, params, _signal, onUpdate) {
			if (options?.operations) unavailable(`${name} tool operations`);
			return yapi.op("builtin.execute", { name, cwd: cwd ?? process.cwd(), toolCallId, params });
		},
	};
}
export const createBashTool = (cwd, options) => builtinTool("bash", cwd, options);
export const createBashToolDefinition = createBashTool;
export const createReadTool = (cwd, options) => builtinTool("read", cwd, options);
export const createReadToolDefinition = createReadTool;
export const createEditTool = (cwd, options) => builtinTool("edit", cwd, options);
export const createEditToolDefinition = createEditTool;
export const createWriteTool = (cwd, options) => builtinTool("write", cwd, options);
export const createWriteToolDefinition = createWriteTool;
export const createGrepTool = (cwd, options) => builtinTool("grep", cwd, options);
export const createGrepToolDefinition = createGrepTool;
export const createFindTool = (cwd, options) => builtinTool("find", cwd, options);
export const createFindToolDefinition = createFindTool;
export const createLsTool = (cwd, options) => builtinTool("ls", cwd, options);
export const createLsToolDefinition = createLsTool;
export const createCodingTools = (cwd) => ["read", "bash", "edit", "write"].map((name) => builtinTool(name, cwd));
export const createReadOnlyTools = (cwd) => ["read", "grep", "find", "ls"].map((name) => builtinTool(name, cwd));
/** pi's local shell: runs `command` with bash, streaming its output to `onData`. */
export function createLocalBashOperations() {
	const { child_process, fs, os } = globalThis.__yapi_builtins;
	return {
		async exec(command, cwd, { onData, signal, timeout, env }) {
			if (signal?.aborted) throw new Error("aborted");
			if (!fs.existsSync(cwd)) throw new Error(`Working directory does not exist: ${cwd}\nCannot execute bash commands.`);
			const shell = fs.existsSync("/bin/bash") ? "/bin/bash" : "sh";
			const child = child_process.spawn(shell, ["-c", command], { cwd, env, stdio: ["ignore", "pipe", "pipe"] });
			let timedOut = false;
			const kill = () => child.kill("SIGKILL");
			const timer = timeout > 0 ? setTimeout(() => ((timedOut = true), kill()), timeout * 1000) : undefined;
			child.stdout.on("data", onData);
			child.stderr.on("data", onData);
			signal?.addEventListener("abort", kill, { once: true });
			try {
				const exitCode = await new Promise((resolve, reject) => {
					child.on("error", reject);
					child.on("close", resolve);
				});
				if (signal?.aborted) throw new Error("aborted");
				if (timedOut) throw new Error(`timeout:${timeout}`);
				// A shell a signal ended reports 128 plus the signal's number, as in pi.
				return { exitCode: exitCode ?? (child.signalCode ? 128 + (os.constants.signals[child.signalCode] ?? 0) : 1) };
			} finally {
				clearTimeout(timer);
				signal?.removeEventListener("abort", kill);
			}
		},
	};
}

// ----- text helpers ----------------------------------------------------------------------
export function formatSize(bytes) {
	if (bytes < 1024) return `${bytes}B`;
	if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
	return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
}
const byteLength = (text) => yapi.utf8Encode(text).length;
function truncate(content, options, fromEnd) {
	const maxLines = options?.maxLines ?? DEFAULT_MAX_LINES;
	const maxBytes = options?.maxBytes ?? DEFAULT_MAX_BYTES;
	const lines = content.split("\n");
	const totalLines = lines.length;
	const totalBytes = byteLength(content);
	if (totalLines <= maxLines && totalBytes <= maxBytes) {
		return { content, truncated: false, truncatedBy: null, totalLines, totalBytes, outputLines: totalLines, outputBytes: totalBytes, lastLinePartial: false, firstLineExceedsLimit: false, maxLines, maxBytes };
	}
	const kept = [];
	let bytes = 0;
	let truncatedBy = "lines";
	const ordered = fromEnd ? [...lines].reverse() : lines;
	for (const line of ordered) {
		if (kept.length >= maxLines) break;
		const size = byteLength(line) + (kept.length > 0 ? 1 : 0);
		if (bytes + size > maxBytes) {
			truncatedBy = "bytes";
			break;
		}
		kept.push(line);
		bytes += size;
	}
	const output = (fromEnd ? kept.reverse() : kept).join("\n");
	return { content: output, truncated: true, truncatedBy, totalLines, totalBytes, outputLines: kept.length, outputBytes: byteLength(output), lastLinePartial: false, firstLineExceedsLimit: kept.length === 0 && totalLines > 0, maxLines, maxBytes };
}
export const truncateHead = (content, options) => truncate(content, options, false);
export const truncateTail = (content, options) => truncate(content, options, true);
export function truncateLine(line, maxChars = 500) {
	return line.length <= maxChars ? { text: line, wasTruncated: false } : { text: `${line.slice(0, maxChars)}... [truncated]`, wasTruncated: true };
}
export function truncateToVisualLines(text, maxLines) {
	const lines = String(text).split("\n");
	return lines.length <= maxLines ? { visualLines: lines, skippedCount: 0 } : { visualLines: lines.slice(-maxLines), skippedCount: lines.length - maxLines };
}
export function estimateTokens(message) {
	const text = typeof message === "string" ? message : JSON.stringify(message?.content ?? message ?? "");
	return Math.ceil(text.length / 4);
}
export function calculateContextTokens(usage) {
	return usage.totalTokens || usage.input + usage.output + usage.cacheRead + usage.cacheWrite;
}
export function getLastAssistantUsage(entries) {
	for (let i = entries.length - 1; i >= 0; i--) {
		const message = entries[i]?.message ?? entries[i];
		if (message?.role === "assistant" && message.usage && message.stopReason !== "aborted" && message.stopReason !== "error") return message.usage;
	}
	return undefined;
}
export function getLatestCompactionEntry(entries) {
	for (let i = entries.length - 1; i >= 0; i--) if (entries[i]?.type === "compaction") return entries[i];
	return undefined;
}
export function createSyntheticSourceInfo(path, options = {}) {
	return { path, source: options.source ?? "local", scope: "temporary", origin: "top-level", baseDir: options.baseDir };
}

export function parseFrontmatter(content) {
	return yapi.request("frontmatter.parse", { content: String(content) });
}
export const stripFrontmatter = (content) => parseFrontmatter(content).body;

const mutationQueues = new Map();
export async function withFileMutationQueue(path, fn) {
	const previous = mutationQueues.get(path) ?? Promise.resolve();
	let release;
	const current = new Promise((resolve) => {
		release = resolve;
	});
	mutationQueues.set(path, previous.then(() => current));
	await previous.catch(() => {});
	try {
		return await fn();
	} finally {
		release();
		if (mutationQueues.get(path) === current) mutationQueues.delete(path);
	}
}

const LANGUAGES = { ts: "typescript", tsx: "typescript", js: "javascript", jsx: "javascript", mjs: "javascript", cjs: "javascript", py: "python", rs: "rust", go: "go", rb: "ruby", java: "java", c: "c", h: "c", cpp: "cpp", hpp: "cpp", cs: "csharp", sh: "bash", bash: "bash", zsh: "bash", json: "json", yaml: "yaml", yml: "yaml", toml: "toml", md: "markdown", html: "html", css: "css", sql: "sql", swift: "swift", kt: "kotlin", php: "php", lua: "lua" };
export function getLanguageFromPath(path) {
	const extension = String(path).split(".").pop()?.toLowerCase();
	return LANGUAGES[extension] ?? undefined;
}
export function highlightCode(code) {
	return String(code).split("\n");
}

// ----- conversation helpers, run by yapi ------------------------------------------------------
export function convertToLlm(messages) {
	return yapi.request("util.convertToLlm", { messages });
}
export function serializeConversation(messages) {
	return yapi.request("util.serializeConversation", { messages });
}
export function copyToClipboard(text) {
	return yapi.op("clipboard.copy", { text });
}

// ----- theme and key hints --------------------------------------------------------------------
const identity = (text) => text;
export const Theme = yapi.Theme;
// pi's `Theme` constructor turns colors into escape sequences as pi-tui does.
yapi.colorAnsi = (value, mode, background) => (background ? backgroundAnsi : foregroundAnsi)(parseColor(value), mode);
const theme = globalThis.__yapi_theme;
export function initTheme() {}
export function getMarkdownTheme() {
	return {
		heading: (text) => theme.fg("mdHeading", text),
		link: (text) => theme.fg("mdLink", text),
		linkUrl: (text) => theme.fg("mdLinkUrl", text),
		code: (text) => theme.fg("mdCode", text),
		codeBlock: (text) => theme.fg("mdCodeBlock", text),
		codeBlockBorder: (text) => theme.fg("mdCodeBlockBorder", text),
		quote: (text) => theme.fg("mdQuote", text),
		quoteBorder: (text) => theme.fg("mdQuoteBorder", text),
		hr: (text) => theme.fg("mdHr", text),
		listBullet: (text) => theme.fg("mdListBullet", text),
		bold: (text) => theme.bold(text),
		italic: (text) => theme.italic(text),
		underline: (text) => theme.underline(text),
		strikethrough: (text) => theme.strikethrough(text),
		highlightCode: (code) => String(code).split("\n").map((line) => theme.fg("mdCodeBlock", line)),
	};
}
export function getSelectListTheme() {
	return {
		selectedPrefix: (text) => theme.fg("accent", text),
		selectedText: (text) => theme.fg("accent", text),
		description: (text) => theme.fg("muted", text),
		scrollInfo: (text) => theme.fg("muted", text),
		noMatch: (text) => theme.fg("muted", text),
	};
}
export function getEditorTheme() {
	return { borderColor: (text) => theme.fg("borderMuted", text), selectList: getSelectListTheme() };
}
export function getSettingsListTheme() {
	return {
		label: (text, selected) => (selected ? theme.fg("accent", text) : text),
		value: (text, selected) => (selected ? theme.fg("accent", text) : theme.fg("muted", text)),
		description: (text) => theme.fg("dim", text),
		get cursor() {
			return theme.fg("accent", "→ ");
		},
		hint: (text) => theme.fg("dim", text),
	};
}
export function keyText(keybinding) {
	return yapi.request("keys.text", { keybinding }) ?? keybinding;
}
export function keyHint(keybinding, description) {
	return `${keyText(keybinding)} ${description}`;
}
export function rawKeyHint(key, description) {
	return `${key} ${description}`;
}

// ----- components ---------------------------------------------------------------------------------
export class DynamicBorder {
	constructor(color = (text) => theme.fg("border", text)) {
		this.color = color;
	}
	invalidate() {}
	render(width) {
		return [this.color("─".repeat(Math.max(1, width)))];
	}
}
export class BorderedLoader extends Container {
	constructor(_tui, _theme, message) {
		super();
		this.signal = new AbortController().signal;
		this.onAbort = undefined;
		this.addChild(new DynamicBorder());
		this.addChild(new Text(message ?? "", 1, 0));
		this.addChild(new DynamicBorder());
	}
	handleInput() {}
	dispose() {}
}
/**
 * pi's `CustomEditor`: the editor with the app's key bindings. The handlers
 * are set when the editor replaces the built-in one; the host then draws
 * the working status in its top border when `embedWorkingStatus` is set.
 */
export class CustomEditor extends Editor {
	constructor(tui, theme, keybindings, options) {
		super(tui, theme, options);
		this.keybindings = keybindings;
		this.embedWorkingStatus = options?.embedWorkingStatus ?? false;
		this.actionHandlers = new Map();
	}
	onAction(action, handler) {
		this.actionHandlers.set(action, handler);
	}
	handleInput(data) {
		if (this.onExtensionShortcut?.(data)) return;
		if (this.keybindings.matches(data, "app.clipboard.pasteImage")) {
			this.onPasteImage?.();
			return;
		}
		if (this.keybindings.matches(data, "app.interrupt")) {
			if (!this.isShowingAutocomplete()) {
				const handler = this.onEscape ?? this.actionHandlers.get("app.interrupt");
				if (handler) {
					handler();
					return;
				}
			}
			super.handleInput(data);
			return;
		}
		if (this.keybindings.matches(data, "app.exit") && this.getText().length === 0) {
			const handler = this.onCtrlD ?? this.actionHandlers.get("app.exit");
			if (handler) handler();
			return;
		}
		if (this.keybindings.matches(data, "tui.editor.historyPrevious") || this.keybindings.matches(data, "tui.editor.historyNext")) {
			super.handleInput(data);
			return;
		}
		for (const [action, handler] of this.actionHandlers) {
			if (action !== "app.interrupt" && action !== "app.exit" && this.keybindings.matches(data, action)) {
				handler();
				return;
			}
		}
		super.handleInput(data);
	}
}

// ----- RPC client ------------------------------------------------------------------------------
/**
 * pi's `RpcClient`: runs another agent in RPC mode and drives it over its
 * standard input and output. The agent is yapi itself, the binary at
 * `process.execPath`, so `cliPath` is ignored.
 */
export class RpcClient {
	constructor(options = {}) {
		this.options = options;
		this.process = null;
		this.eventListeners = [];
		this.pendingRequests = new Map();
		this.requestId = 0;
		this.stderr = "";
		this.exitError = null;
	}
	async start() {
		if (this.process) throw new Error("Client already started");
		this.exitError = null;
		const args = ["--mode", "rpc"];
		if (this.options.provider) args.push("--provider", this.options.provider);
		if (this.options.model) args.push("--model", this.options.model);
		if (this.options.args) args.push(...this.options.args);
		const child = globalThis.__yapi_builtins.child_process.spawn(process.execPath, args, {
			cwd: this.options.cwd,
			env: { ...process.env, ...this.options.env },
			stdio: ["pipe", "pipe", "pipe"],
		});
		this.process = child;
		const fail = (error) => {
			if (this.process !== child) return;
			this.exitError = error;
			for (const pending of this.pendingRequests.values()) pending.reject(error);
			this.pendingRequests.clear();
		};
		child.stderr.on("data", (data) => {
			this.stderr += data.toString();
			process.stderr.write(data);
		});
		child.once("exit", (code, signal) => fail(this.exitedError(code, signal)));
		child.once("error", (error) => fail(new Error(`Agent process error: ${error.message}. Stderr: ${this.stderr}`)));
		// Strict JSONL: records end at LF only.
		let buffer = "";
		child.stdout.setEncoding("utf8");
		child.stdout.on("data", (chunk) => {
			buffer += chunk;
			for (let end; this.process === child && (end = buffer.indexOf("\n")) !== -1; buffer = buffer.slice(end + 1)) {
				this.handleLine(buffer.slice(0, end).replace(/\r$/, ""));
			}
		});
		await new Promise((resolve) => setTimeout(resolve, 100));
		if (this.exitError) throw this.exitError;
		if (child.exitCode !== null) throw (this.exitError = this.exitedError(child.exitCode, child.signalCode));
	}
	async stop() {
		const child = this.process;
		if (!child) return;
		child.kill("SIGTERM");
		await new Promise((resolve) => {
			const timeout = setTimeout(() => {
				child.kill("SIGKILL");
				resolve();
			}, 1000);
			child.on("exit", () => {
				clearTimeout(timeout);
				resolve();
			});
		});
		this.process = null;
		this.pendingRequests.clear();
	}
	onEvent(listener) {
		this.eventListeners.push(listener);
		return () => {
			const index = this.eventListeners.indexOf(listener);
			if (index !== -1) this.eventListeners.splice(index, 1);
		};
	}
	getStderr() {
		return this.stderr;
	}
	/** Events until the agent settles; `prompt` sends one first. */
	collectEvents(timeout = 60000) {
		return new Promise((resolve, reject) => {
			const events = [];
			const timer = setTimeout(() => {
				unsubscribe();
				reject(new Error(`Timeout collecting events. Stderr: ${this.stderr}`));
			}, timeout);
			const unsubscribe = this.onEvent((event) => {
				events.push(event);
				if (event.type !== "agent_settled") return;
				clearTimeout(timer);
				unsubscribe();
				resolve(events);
			});
		});
	}
	waitForIdle(timeout = 60000) {
		return this.collectEvents(timeout).then(() => undefined, (error) => {
			throw new Error(error.message.replace("collecting events", "waiting for agent to become idle"));
		});
	}
	async promptAndWait(message, images, timeout = 60000) {
		const events = this.collectEvents(timeout);
		await this.prompt(message, images);
		return events;
	}
	handleLine(line) {
		let data;
		try {
			data = JSON.parse(line);
		} catch {
			return;
		}
		const pending = data.type === "response" && data.id ? this.pendingRequests.get(data.id) : undefined;
		if (pending) {
			this.pendingRequests.delete(data.id);
			pending.resolve(data);
			return;
		}
		for (const listener of [...this.eventListeners]) listener(data);
	}
	exitedError(code, signal) {
		return new Error(`Agent process exited (code=${code} signal=${signal}). Stderr: ${this.stderr}`);
	}
	send(command) {
		const child = this.process;
		if (!child?.stdin) return Promise.reject(new Error("Client not started"));
		if (this.exitError) return Promise.reject(this.exitError);
		if (child.exitCode !== null) return Promise.reject((this.exitError = this.exitedError(child.exitCode, child.signalCode)));
		const id = `req_${++this.requestId}`;
		return new Promise((resolve, reject) => {
			const timeout = setTimeout(() => {
				this.pendingRequests.delete(id);
				reject(new Error(`Timeout waiting for response to ${command.type}. Stderr: ${this.stderr}`));
			}, 30000);
			const settle = (finish) => (value) => {
				clearTimeout(timeout);
				finish(value);
			};
			this.pendingRequests.set(id, { resolve: settle(resolve), reject: settle(reject) });
			child.stdin.write(`${JSON.stringify({ ...command, id })}\n`);
		});
	}
}
/**
 * RpcClient's commands, as pi defines them: the method, the RPC command, its
 * arguments in order, and what the method returns: nothing (`undefined`),
 * the response's data (`null`), or one field of it.
 */
const RPC_COMMANDS = [
	["prompt", "prompt", ["message", "images", "streamingBehavior"], "disposition"],
	["steer", "steer", ["message", "images"], "disposition"],
	["followUp", "follow_up", ["message", "images"], "disposition"],
	["abort", "abort", []],
	["clearQueue", "clear_queue", [], null],
	["newSession", "new_session", ["parentSession"], null],
	["getState", "get_state", [], null],
	["setModel", "set_model", ["provider", "modelId"], null],
	["cycleModel", "cycle_model", [], null],
	["getAvailableModels", "get_available_models", [], "models"],
	["setThinkingLevel", "set_thinking_level", ["level"]],
	["cycleThinkingLevel", "cycle_thinking_level", [], null],
	["getAvailableThinkingLevels", "get_available_thinking_levels", [], "levels"],
	["setSteeringMode", "set_steering_mode", ["mode"]],
	["setFollowUpMode", "set_follow_up_mode", ["mode"]],
	["compact", "compact", ["customInstructions"], null],
	["setAutoCompaction", "set_auto_compaction", ["enabled"]],
	["setAutoRetry", "set_auto_retry", ["enabled"]],
	["abortRetry", "abort_retry", []],
	["bash", "bash", ["command"], null],
	["abortBash", "abort_bash", []],
	["getSessionStats", "get_session_stats", [], null],
	["exportHtml", "export_html", ["outputPath"], null],
	["switchSession", "switch_session", ["sessionPath"], null],
	["fork", "fork", ["entryId"], null],
	["clone", "clone", [], null],
	["getForkMessages", "get_fork_messages", [], "messages"],
	["getEntries", "get_entries", ["since"], null],
	["getTree", "get_tree", [], null],
	["getLastAssistantText", "get_last_assistant_text", [], "text"],
	["setSessionName", "set_session_name", ["name"]],
	["getMessages", "get_messages", [], "messages"],
	["getCommands", "get_commands", [], "commands"],
];
for (const [method, type, keys, returns] of RPC_COMMANDS) {
	RpcClient.prototype[method] = async function (...values) {
		const response = await this.send({ type, ...Object.fromEntries(keys.map((key, index) => [key, values[index]])) });
		if (returns === undefined) return;
		if (!response.success) throw new Error(response.error);
		return returns === null ? response.data : response.data?.[returns];
	};
}

// ----- not available in yapi --------------------------------------------------------------------------
export const { AgentSession, AgentSessionRuntime, ArminComponent, AssistantMessageComponent, BashExecutionComponent, BranchSummaryMessageComponent,
	CompactionSummaryMessageComponent, CredentialSynchronizationError, CustomMessageComponent, DefaultPackageManager, DefaultResourceLoader,
	ExtensionEditorComponent, ExtensionInputComponent, ExtensionRunner, ExtensionSelectorComponent, FooterComponent, InteractiveMode,
	LoginDialogComponent, ModelRegistry, ModelRuntime, ModelSelectorComponent, OAuthSelectorComponent, ProjectTrustStore, SessionManager,
	SessionSelectorComponent, SettingsManager, SettingsSelectorComponent, ShowImagesSelectorComponent, SkillInvocationMessageComponent,
	ThemeSelectorComponent, ThinkingSelectorComponent, ToolExecutionComponent, TreeSelectorComponent, UserMessageComponent,
	UserMessageSelectorComponent, buildContextEntries, buildSessionContext, buildSessionProjection, collectEntriesForBranchSummary, compact,
	convertToPng, createAgentSession, createAgentSessionFromServices, createAgentSessionRuntime, createAgentSessionServices, createExtensionRuntime,
	createLocalPowerShellOperations, createMcpExtension, createPowerShellTool, createPowerShellToolDefinition, createToolSearchExtension,
	detectSupportedImageMimeTypeFromFile, discoverAndLoadExtensions, findCutPoint, findTurnStartIndex, formatDimensionNote, formatSkillsForPrompt,
	generateBranchSummary, generateDiffString, generateSummary, generateSummaryWithUsage, generateUnifiedPatch, getPowerShellConfig,
	hasTrustRequiringProjectResources, loadProjectContextFiles, loadSkills, loadSkillsFromDir, main, migrateSessionEntries, parseArgs,
	parseSessionEntries, parseSkillBlock, prepareBranchEntries, readStoredCredential, renderDiff, resizeImage, resolveCliModel,
	resolveModelScopeWithDiagnostics, runPrintMode, runRpcMode, sessionEntryToContextMessages, shouldCompact, wrapRegisteredTool, wrapRegisteredTools } = yapi.stubs("@earendil-works/pi-coding-agent");
/**
 * pi's codemode extension, for packages that decorate the tool before
 * registering it. yapi runs the scripts; `mode`, `inlineBudget` and `models`
 * options are ignored.
 */
export function createCodemodeExtension() {
	return (pi) => {
		const info = yapi.request("codemode.definition", {});
		pi.registerTool({
			...info,
			exposure: "model-only",
			defaultActive: false,
			async execute(toolCallId, params) {
				return yapi.op("codemode.execute", { toolCallId, params });
			},
		});
	};
}
