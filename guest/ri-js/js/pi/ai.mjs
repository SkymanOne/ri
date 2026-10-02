// `@earendil-works/pi-ai` and its subpaths for extensions in ri. Helpers are
// ports of pi-ai 1.0.0 (MIT, Copyright (c) Mario Zechner); completions run on
// ri's providers through the host. Stubs cover the rest of pi-ai's exports so
// every import links; a stub throws when called.
import { Type } from "typebox";
import { Value } from "typebox/value";

const ri = globalThis.__ri;

function unavailable(name) {
	const error = new Error(`${name} from @earendil-works/pi-ai is not available in ri extensions`);
	error.code = "ERR_NOT_SUPPORTED";
	throw error;
}

export { Type };

export function StringEnum(values, options) {
	return Type.Unsafe({
		type: "string",
		enum: values,
		...(options?.description && { description: options.description }),
		...(options?.default && { default: options.default }),
	});
}

// ----- ids ---------------------------------------------------------------------------
const MAX_UUID_V7_TIMESTAMP = 0xffffffffffff;
const MAX_SEQUENCE = (1n << 41n) - 1n;
let lastOrdinaryTimestamp = -1;
let sequence;

export function uuidv7(timestampMs) {
	const requestedTimestamp = timestampMs ?? Date.now();
	if (!Number.isInteger(requestedTimestamp) || requestedTimestamp < 0 || requestedTimestamp > MAX_UUID_V7_TIMESTAMP) {
		throw new RangeError(`UUIDv7 timestamp must be an integer between 0 and ${MAX_UUID_V7_TIMESTAMP}`);
	}
	const effectiveTimestamp = timestampMs === undefined ? Math.max(requestedTimestamp, lastOrdinaryTimestamp) : timestampMs;
	if (timestampMs === undefined) lastOrdinaryTimestamp = effectiveTimestamp;
	const bytes = new Uint8Array(16);
	globalThis.crypto.getRandomValues(bytes);
	if (sequence === undefined) {
		sequence = (BigInt(bytes[1]) << 32n) | (BigInt(bytes[2]) << 24n) | (BigInt(bytes[3]) << 16n) | (BigInt(bytes[4]) << 8n) | BigInt(bytes[5]);
	} else {
		if (sequence === MAX_SEQUENCE) throw new RangeError("UUIDv7 generator sequence exhausted");
		sequence++;
	}
	const timestamp = BigInt(effectiveTimestamp);
	for (let index = 5; index >= 0; index--) bytes[index] = Number(timestamp >> BigInt((5 - index) * 8)) & 0xff;
	bytes[6] = 0x70 | Number((sequence >> 37n) & 0x0fn);
	bytes[7] = Number((sequence >> 29n) & 0xffn);
	bytes[8] = 0x80 | Number((sequence >> 23n) & 0x3fn);
	bytes[9] = Number((sequence >> 15n) & 0xffn);
	bytes[10] = Number((sequence >> 7n) & 0xffn);
	bytes[11] = Number((sequence & 0x7fn) << 1n) | (bytes[11] & 0x01);
	const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0"));
	return `${hex.slice(0, 4).join("")}-${hex.slice(4, 6).join("")}-${hex.slice(6, 8).join("")}-${hex.slice(8, 10).join("")}-${hex.slice(10).join("")}`;
}

// ----- event streams -------------------------------------------------------------------------
export class EventStream {
	#queue = [];
	#waiting = [];
	#done = false;
	#resolveFinal;
	#final;
	constructor(isComplete, extractResult) {
		this.isComplete = isComplete;
		this.extractResult = extractResult;
		this.#final = new Promise((resolve) => {
			this.#resolveFinal = resolve;
		});
	}
	push(event) {
		if (this.#done) return;
		if (this.isComplete(event)) {
			this.#done = true;
			this.#resolveFinal(this.extractResult(event));
		}
		const waiter = this.#waiting.shift();
		if (waiter) waiter({ value: event, done: false });
		else this.#queue.push(event);
	}
	end(result) {
		this.#done = true;
		if (result !== undefined) this.#resolveFinal(result);
		while (this.#waiting.length > 0) this.#waiting.shift()({ value: undefined, done: true });
	}
	async *[Symbol.asyncIterator]() {
		while (true) {
			if (this.#queue.length > 0) yield this.#queue.shift();
			else if (this.#done) return;
			else {
				const result = await new Promise((resolve) => this.#waiting.push(resolve));
				if (result.done) return;
				yield result.value;
			}
		}
	}
	result() {
		return this.#final;
	}
}

export class AssistantMessageEventStream extends EventStream {
	constructor() {
		super(
			(event) => event.type === "done" || event.type === "error",
			(event) => {
				if (event.type === "done") return event.message;
				if (event.type === "error") return event.error;
				throw new Error("Unexpected event type for final result");
			},
		);
	}
}

export function createAssistantMessageEventStream() {
	return new AssistantMessageEventStream();
}

// ----- completions on ri's providers --------------------------------------------------------------
export function streamSimple(model, context, options) {
	const stream = new AssistantMessageEventStream();
	const { signal, onPayload, ...rest } = options ?? {};
	ri.op("ai.complete", { model, context, options: rest }).then(
		(message) => {
			stream.push({ type: "start", partial: message });
			stream.push(message.stopReason === "error" || message.stopReason === "aborted" ? { type: "error", reason: message.stopReason, error: message } : { type: "done", reason: message.stopReason, message });
		},
		(error) => stream.push({ type: "error", reason: "error", error: { role: "assistant", content: [], api: model.api, provider: model.provider, model: model.id, usage: emptyUsage(), stopReason: "error", errorMessage: error.message, timestamp: Date.now() } }),
	);
	return stream;
}
export const stream = streamSimple;
export const complete = (model, context, options) => streamSimple(model, context, options).result();
export const completeSimple = complete;

const emptyUsage = () => ({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } });

// ----- models ------------------------------------------------------------------------------------
const THINKING_LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

export function calculateCost(model, usage) {
	const inputTokens = usage.input + usage.cacheRead + usage.cacheWrite;
	let rates = model.cost;
	let matchedThreshold = -1;
	for (const tier of model.cost.tiers ?? []) {
		if (inputTokens > tier.inputTokensAbove && tier.inputTokensAbove > matchedThreshold) {
			rates = tier;
			matchedThreshold = tier.inputTokensAbove;
		}
	}
	const longWrite = usage.cacheWrite1h ?? 0;
	const shortWrite = usage.cacheWrite - longWrite;
	usage.cost.input = (rates.input / 1000000) * usage.input;
	usage.cost.output = (rates.output / 1000000) * usage.output;
	usage.cost.cacheRead = (rates.cacheRead / 1000000) * usage.cacheRead;
	usage.cost.cacheWrite = (rates.cacheWrite * shortWrite + rates.input * 2 * longWrite) / 1000000;
	usage.cost.total = usage.cost.input + usage.cost.output + usage.cost.cacheRead + usage.cost.cacheWrite;
	return usage.cost;
}

export function getSupportedThinkingLevels(model) {
	if (!model.reasoning) return ["off"];
	return THINKING_LEVELS.filter((level) => {
		const mapped = model.thinkingLevelMap?.[level];
		if (mapped === null) return false;
		if (level === "xhigh" || level === "max") return mapped !== undefined;
		return true;
	});
}

export function clampThinkingLevel(model, level) {
	const available = getSupportedThinkingLevels(model);
	if (available.includes(level)) return level;
	const requested = THINKING_LEVELS.indexOf(level);
	if (requested === -1) return available[0] ?? "off";
	for (let i = requested; i < THINKING_LEVELS.length; i++) if (available.includes(THINKING_LEVELS[i])) return THINKING_LEVELS[i];
	for (let i = requested - 1; i >= 0; i--) if (available.includes(THINKING_LEVELS[i])) return THINKING_LEVELS[i];
	return available[0] ?? "off";
}

export const getModelType = (model) => model.type ?? "llm";
export const isModelType = (model, type) => getModelType(model) === type;
export function modelsAreEqual(a, b) {
	if (!a || !b) return false;
	return getModelType(a) === getModelType(b) && a.id === b.id && a.provider === b.provider;
}
export const getModel = (provider, id) => ri.request("models.find", { provider, id }) ?? undefined;
export const getModels = (provider) => ri.request("models.list", { provider });
export const getProviders = () => ri.request("models.providers");
export const getEnvApiKey = (provider) => ri.request("models.envApiKey", { provider }) ?? undefined;

// ----- text and JSON -------------------------------------------------------------------------------
export function contentText(content, separator = "\n") {
	if (typeof content === "string") return content;
	return content
		.filter((block) => block.type === "text")
		.map((block) => block.text)
		.join(separator);
}

export function formatThrownValue(value) {
	if (value instanceof Error) return value.message;
	if (typeof value === "string") return value;
	try {
		return JSON.stringify(value);
	} catch {
		return String(value);
	}
}

export const repairJson = (json) => ri.request("json.repair", { text: json });
export const parseJsonWithRepair = (json) => ri.request("json.parseWithRepair", { text: json });
export const parseStreamingJson = (partial) => ri.request("json.partial", { text: partial ?? "" });

// ----- tool validation ------------------------------------------------------------------------------
export function validateToolArguments(tool, toolCall) {
	const args = structuredClone(toolCall.arguments ?? {});
	Value.Convert(tool.parameters, args);
	if (Value.Check(tool.parameters, args)) return args;
	const errors = [...Value.Errors(tool.parameters, args)].map((error) => `  - ${error.instancePath || "root"}: ${error.message}`).join("\n");
	throw new Error(`Validation failed for tool "${toolCall.name}":\n${errors}\n\nReceived arguments:\n${JSON.stringify(toolCall.arguments, null, 2)}`);
}
export function validateToolCall(tools, toolCall) {
	const tool = tools.find((candidate) => candidate.name === toolCall.name);
	if (!tool) throw new Error(`Tool "${toolCall.name}" not found`);
	return validateToolArguments(tool, toolCall);
}

export const DEFAULT_MAX_AGENT_RETRY_DELAY_MS = 60_000;

// ----- not available in ri ---------------------------------------------------------------------------
export const ANTHROPIC_API_KEY_ENV = "ANTHROPIC_API_KEY";
export const ANTHROPIC_AUTH_TOKEN_ENV = "ANTHROPIC_AUTH_TOKEN";
export const ANTHROPIC_FEDERATION_RULE_ID_ENV = "ANTHROPIC_FEDERATION_RULE_ID";
export const ANTHROPIC_IDENTITY_TOKEN_FILE_ENV = "ANTHROPIC_IDENTITY_TOKEN_FILE";
export const ANTHROPIC_OAUTH_TOKEN_ENV = "ANTHROPIC_OAUTH_TOKEN";
export const ANTHROPIC_ORGANIZATION_ID_ENV = "ANTHROPIC_ORGANIZATION_ID";
export const ANTHROPIC_SERVICE_ACCOUNT_ID_ENV = "ANTHROPIC_SERVICE_ACCOUNT_ID";
export const ANTHROPIC_WORKSPACE_ID_ENV = "ANTHROPIC_WORKSPACE_ID";
export class AssistantMessageFrameEncoder {
	constructor() {
		unavailable("AssistantMessageFrameEncoder");
	}
}
export class InMemoryCredentialStore {
	constructor() {
		unavailable("InMemoryCredentialStore");
	}
}
export class InMemoryModelsStore {
	constructor() {
		unavailable("InMemoryModelsStore");
	}
}
export class ModelsError {
	constructor() {
		unavailable("ModelsError");
	}
}
export function anthropicMessagesApi() {
	return unavailable("anthropicMessagesApi");
}
export function appendAssistantMessageDiagnostic() {
	return unavailable("appendAssistantMessageDiagnostic");
}
export function azureOpenAIResponsesApi() {
	return unavailable("azureOpenAIResponsesApi");
}
export function bedrockConverseStreamApi() {
	return unavailable("bedrockConverseStreamApi");
}
export function cleanupSessionResources() {
	return unavailable("cleanupSessionResources");
}
export function collapseSystemMessages() {
	return unavailable("collapseSystemMessages");
}
export function createAssistantMessageDiagnostic() {
	return unavailable("createAssistantMessageDiagnostic");
}
export function createFauxCore() {
	return unavailable("createFauxCore");
}
export function createInitialSystemMessage() {
	return unavailable("createInitialSystemMessage");
}
export function createModels() {
	return unavailable("createModels");
}
export function createProvider() {
	return unavailable("createProvider");
}
export function declarationsEqual() {
	return unavailable("declarationsEqual");
}
export function defaultProviderAuthContext() {
	return unavailable("defaultProviderAuthContext");
}
export function envApiKeyAuth() {
	return unavailable("envApiKeyAuth");
}
export function extractDiagnosticError() {
	return unavailable("extractDiagnosticError");
}
export function fauxAssistantMessage() {
	return unavailable("fauxAssistantMessage");
}
export function fauxProvider() {
	return unavailable("fauxProvider");
}
export function fauxText() {
	return unavailable("fauxText");
}
export function fauxThinking() {
	return unavailable("fauxThinking");
}
export function fauxToolCall() {
	return unavailable("fauxToolCall");
}
export function findEnvKeys() {
	return unavailable("findEnvKeys");
}
export function generateImages() {
	return unavailable("generateImages");
}
export function generateImagesOpenRouter() {
	return unavailable("generateImagesOpenRouter");
}
export function getApiProvider() {
	return unavailable("getApiProvider");
}
export function getApiProviders() {
	return unavailable("getApiProviders");
}
export function getCurrentSystemMessage() {
	return unavailable("getCurrentSystemMessage");
}
export function getCurrentSystemPrompt() {
	return unavailable("getCurrentSystemPrompt");
}
export function getCurrentTools() {
	return unavailable("getCurrentTools");
}
export function getDeclaredTools() {
	return unavailable("getDeclaredTools");
}
export function getImageModel() {
	return unavailable("getImageModel");
}
export function getImageModels() {
	return unavailable("getImageModels");
}
export function getImageProviders() {
	return unavailable("getImageProviders");
}
export function getImagesApiProvider() {
	return unavailable("getImagesApiProvider");
}
export function getInitialSystemMessage() {
	return unavailable("getInitialSystemMessage");
}
export function getOverflowPatterns() {
	return unavailable("getOverflowPatterns");
}
export function getSystemMessageText() {
	return unavailable("getSystemMessageText");
}
export function getToolStateChanges() {
	return unavailable("getToolStateChanges");
}
export function googleGenerativeAIApi() {
	return unavailable("googleGenerativeAIApi");
}
export function googleVertexApi() {
	return unavailable("googleVertexApi");
}
export function hasApi() {
	return unavailable("hasApi");
}
export function hasNonAdditiveToolChanges() {
	return unavailable("hasNonAdditiveToolChanges");
}
export function hasToolRedefinitions() {
	return unavailable("hasToolRedefinitions");
}
export function isContextOverflow() {
	return unavailable("isContextOverflow");
}
export function isRecoverableLength() {
	return unavailable("isRecoverableLength");
}
export function isRetryableAssistantError() {
	return unavailable("isRetryableAssistantError");
}
export function lazyApi() {
	return unavailable("lazyApi");
}
export function lazyOAuth() {
	return unavailable("lazyOAuth");
}
export function lazyStream() {
	return unavailable("lazyStream");
}
export function mistralConversationsApi() {
	return unavailable("mistralConversationsApi");
}
export function normalizeContext() {
	return unavailable("normalizeContext");
}
export function openAICodexResponsesApi() {
	return unavailable("openAICodexResponsesApi");
}
export function openAICompletionsApi() {
	return unavailable("openAICompletionsApi");
}
export function openAIResponsesApi() {
	return unavailable("openAIResponsesApi");
}
export function piMessagesApi() {
	return unavailable("piMessagesApi");
}
export function reduceAssistantMessageFrames() {
	return unavailable("reduceAssistantMessageFrames");
}
export function registerApiProvider() {
	return unavailable("registerApiProvider");
}
export function registerBuiltInApiProviders() {
	return unavailable("registerBuiltInApiProviders");
}
export function registerBuiltInImagesApiProviders() {
	return unavailable("registerBuiltInImagesApiProviders");
}
export function registerFauxProvider() {
	return unavailable("registerFauxProvider");
}
export function registerImagesApiProvider() {
	return unavailable("registerImagesApiProvider");
}
export function registerSessionResourceCleanup() {
	return unavailable("registerSessionResourceCleanup");
}
export function renderSystemMessageUpdate() {
	return unavailable("renderSystemMessageUpdate");
}
export function resetApiProviders() {
	return unavailable("resetApiProviders");
}
export function resolveTranscript() {
	return unavailable("resolveTranscript");
}
export function resolveTranscriptTools() {
	return unavailable("resolveTranscriptTools");
}
export function retryAssistantCall() {
	return unavailable("retryAssistantCall");
}
export function retryDelayMs() {
	return unavailable("retryDelayMs");
}
export function setBedrockProviderModule() {
	return unavailable("setBedrockProviderModule");
}
export function streamAnthropic() {
	return unavailable("streamAnthropic");
}
export function streamAzureOpenAIResponses() {
	return unavailable("streamAzureOpenAIResponses");
}
export function streamGoogle() {
	return unavailable("streamGoogle");
}
export function streamGoogleVertex() {
	return unavailable("streamGoogleVertex");
}
export function streamMistral() {
	return unavailable("streamMistral");
}
export function streamOpenAICodexResponses() {
	return unavailable("streamOpenAICodexResponses");
}
export function streamOpenAICompletions() {
	return unavailable("streamOpenAICompletions");
}
export function streamOpenAIResponses() {
	return unavailable("streamOpenAIResponses");
}
export function streamSimpleAnthropic() {
	return unavailable("streamSimpleAnthropic");
}
export function streamSimpleAzureOpenAIResponses() {
	return unavailable("streamSimpleAzureOpenAIResponses");
}
export function streamSimpleGoogle() {
	return unavailable("streamSimpleGoogle");
}
export function streamSimpleGoogleVertex() {
	return unavailable("streamSimpleGoogleVertex");
}
export function streamSimpleMistral() {
	return unavailable("streamSimpleMistral");
}
export function streamSimpleOpenAICodexResponses() {
	return unavailable("streamSimpleOpenAICodexResponses");
}
export function streamSimpleOpenAICompletions() {
	return unavailable("streamSimpleOpenAICompletions");
}
export function streamSimpleOpenAIResponses() {
	return unavailable("streamSimpleOpenAIResponses");
}
export function toToolDeclaration() {
	return unavailable("toToolDeclaration");
}
export function unregisterApiProviders() {
	return unavailable("unregisterApiProviders");
}
export function withoutInitialSystemMessage() {
	return unavailable("withoutInitialSystemMessage");
}
