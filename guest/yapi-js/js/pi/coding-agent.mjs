// `@earendil-works/pi-coding-agent` for extensions in yapi: the API helpers
// extensions use, built-in tools backed by yapi's own, and stubs for the rest of
// pi's exports so every import links. A stub throws when called.
import { Container, Editor, Text } from "@earendil-works/pi-tui";

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

export function createEventBus() {
	const handlers = new Map();
	return {
		emit(channel, data) {
			for (const handler of [...(handlers.get(channel) ?? [])]) {
				Promise.resolve()
					.then(() => handler(data))
					.catch((error) => console.error(`Event handler error (${channel}):`, error));
			}
		},
		on(channel, handler) {
			handlers.set(channel, [...(handlers.get(channel) ?? []), handler]);
			return () => handlers.set(channel, (handlers.get(channel) ?? []).filter((item) => item !== handler));
		},
		clear() {
			handlers.clear();
		},
	};
}

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
export function createLocalBashOperations() {
	return {
		exec: (command, cwd, options) => yapi.op("exec", { command: "/bin/sh", args: ["-c", command], cwd, timeout: options?.timeout }),
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
export class CustomEditor extends Editor {
	constructor(tui, theme, keybindings, options) {
		super(tui, theme, options);
		this.keybindings = keybindings;
		this.actionHandlers = new Map();
	}
	onAction(action, handler) {
		this.actionHandlers.set(action, handler);
	}
}

// ----- not available in yapi --------------------------------------------------------------------------
export const { AgentSession, AgentSessionRuntime, ArminComponent, AssistantMessageComponent, BashExecutionComponent, BranchSummaryMessageComponent,
	CompactionSummaryMessageComponent, CredentialSynchronizationError, CustomMessageComponent, DefaultPackageManager, DefaultResourceLoader,
	ExtensionEditorComponent, ExtensionInputComponent, ExtensionRunner, ExtensionSelectorComponent, FooterComponent, InteractiveMode,
	LoginDialogComponent, ModelRegistry, ModelRuntime, ModelSelectorComponent, OAuthSelectorComponent, ProjectTrustStore, RpcClient, SessionManager,
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
