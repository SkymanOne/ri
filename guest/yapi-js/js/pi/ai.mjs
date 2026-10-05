// `@earendil-works/pi-ai` and its subpaths for extensions in yapi. Helpers are
// ports of pi-ai 1.0.0 (MIT, Copyright (c) Mario Zechner); completions run on
// yapi's providers through the host. Stubs cover the rest of pi-ai's exports so
// every import links; a stub throws when called.
import { Type } from "typebox";
import { Value } from "typebox/value";

const yapi = globalThis.__yapi;

function unavailable(name) {
	const error = new Error(`${name} from @earendil-works/pi-ai is not available in yapi extensions`);
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

// ----- completions on yapi's providers --------------------------------------------------------------
function hostStream(model, context, options) {
	const stream = new AssistantMessageEventStream();
	const { signal, onPayload, ...rest } = options ?? {};
	yapi.op("ai.complete", { model, context, options: rest }).then(
		(message) => {
			stream.push({ type: "start", partial: message });
			stream.push(message.stopReason === "error" || message.stopReason === "aborted" ? { type: "error", reason: message.stopReason, error: message } : { type: "done", reason: message.stopReason, message });
		},
		(error) => stream.push({ type: "error", reason: "error", error: { role: "assistant", content: [], api: model.api, provider: model.provider, model: model.id, usage: emptyUsage(), stopReason: "error", errorMessage: error.message, timestamp: Date.now() } }),
	);
	return stream;
}
/** Every wire API runs on yapi's provider for the model's `api`. */
const hostApi = { stream: hostStream, streamSimple: hostStream };

// ----- API providers -----------------------------------------------------------------------------
// pi-ai's registry. A stream an extension registers for an API serves its
// own calls to `stream` and `streamSimple`. yapi's agent runs a session's model
// on yapi's providers, so a model whose API only an extension implements cannot
// be the session's model.
const BUILTIN_APIS = [
	"anthropic-messages",
	"openai-completions",
	"openai-responses",
	"openai-codex-responses",
	"azure-openai-responses",
	"google-generative-ai",
	"google-vertex",
	"mistral-conversations",
	"bedrock-converse-stream",
];
const apiProviders = new Map();
const forApi = (api, fn) => (model, context, options) => {
	if (model.api !== api) throw new Error(`Mismatched api: ${model.api} expected ${api}`);
	return fn(model, context, options);
};
export function registerApiProvider(provider, sourceId) {
	apiProviders.set(provider.api, {
		provider: { api: provider.api, stream: forApi(provider.api, provider.stream), streamSimple: forApi(provider.api, provider.streamSimple) },
		sourceId,
	});
}
export function getApiProvider(api) {
	return apiProviders.get(api)?.provider ?? (BUILTIN_APIS.includes(api) ? { api, ...hostApi } : undefined);
}
export const getApiProviders = () => Array.from(apiProviders.values(), (entry) => entry.provider);
export function unregisterApiProviders(sourceId) {
	for (const [api, entry] of apiProviders) if (entry.sourceId === sourceId) apiProviders.delete(api);
}
export function resetApiProviders() {
	apiProviders.clear();
}
export function registerBuiltInApiProviders() {}
export function streamSimple(model, context, options) {
	const custom = apiProviders.get(model?.api);
	return custom ? custom.provider.streamSimple(model, context, options) : hostStream(model, context, options);
}
export function stream(model, context, options) {
	const custom = apiProviders.get(model?.api);
	return custom ? custom.provider.stream(model, context, options) : hostStream(model, context, options);
}
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

export const getModelType = (model) => model.type ?? "chat";
export const isModelType = (model, type) => getModelType(model) === type;
export function modelsAreEqual(a, b) {
	if (!a || !b) return false;
	return getModelType(a) === getModelType(b) && a.id === b.id && a.provider === b.provider;
}
export const getModel = (provider, id) => yapi.request("models.builtin", { provider, id }) ?? undefined;
export const getModels = (provider) => yapi.request("models.list", { provider });
export const getProviders = () => yapi.request("models.providers");
export const getEnvApiKey = (provider) => yapi.request("models.envApiKey", { provider }) ?? undefined;

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

export const repairJson = (json) => yapi.request("json.repair", { text: json });
export const parseJsonWithRepair = (json) => yapi.request("json.parseWithRepair", { text: json });
export const parseStreamingJson = (partial) => yapi.request("json.partial", { text: partial ?? "" });

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

// ----- not available in yapi ---------------------------------------------------------------------------
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
export class ModelsError extends Error {
	constructor(kind, message, options) {
		super(message, options);
		this.name = "ModelsError";
		this.kind = kind;
	}
}
export function anthropicMessagesApi() {
	return hostApi;
}
export function appendAssistantMessageDiagnostic() {
	return unavailable("appendAssistantMessageDiagnostic");
}
export function azureOpenAIResponsesApi() {
	return hostApi;
}
export function bedrockConverseStreamApi() {
	return hostApi;
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
const KNOWN_MODEL_TYPES = ["chat", "image", "classifier"];
export function createProvider(input) {
	const single = input.api && typeof input.api.stream === "function" ? input.api : undefined;
	const byApi = single || !input.api ? undefined : input.api;
	const images = input.images;
	const classifiers = input.classifiers;
	const streams = single ? [single] : Object.values(byApi ?? {}).filter((entry) => entry !== undefined);
	const imageImplementations = Object.values(images ?? {}).filter((entry) => entry !== undefined);
	const classifierImplementations = Object.values(classifiers ?? {}).filter((entry) => entry !== undefined);
	if (streams.length === 0 && imageImplementations.length === 0 && classifierImplementations.length === 0) {
		throw new Error(`Provider ${input.id}: at least one of "api", "images", or "classifiers" is required.`);
	}
	const baselineModels = input.models;
	let dynamicModels = [];
	const fetchModels = input.fetchModels;
	const currentModels = () => {
		const merged = [...baselineModels];
		for (const model of dynamicModels) {
			const index = merged.findIndex((entry) => getModelType(entry) === getModelType(model) && entry.id === model.id);
			if (index >= 0) merged[index] = model;
			else merged.push(model);
		}
		return merged;
	};
	const apiFor = (model) => single ?? byApi?.[model.api];
	const dispatch = (model, run) => {
		const implementation = apiFor(model);
		if (!implementation) {
			return lazyStream(model, async () => {
				throw new ModelsError("stream", `Provider ${input.id} has no API implementation for "${model.api}"`);
			});
		}
		return run(implementation);
	};
	const provider = {
		id: input.id,
		name: input.name ?? input.id,
		baseUrl: input.baseUrl,
		headers: input.headers,
		auth: input.auth,
		getModels: () => currentModels().filter((model) => isModelType(model, "chat")),
		getAllModels: currentModels,
		refreshModels: fetchModels
			? async (context) => {
					if (context.stored) {
						const restored = context.stored.models.filter((model) => model.provider === input.id);
						if (!(await context.publish({ update: () => void (dynamicModels = restored) }))) return;
					}
					if (!context.allowNetwork || context.signal.aborted) return;
					const fetched = await fetchModels(context);
					if (context.signal.aborted) return;
					const refreshed = fetched.filter((model) => KNOWN_MODEL_TYPES.includes(getModelType(model)));
					await context.publish({ persist: { models: refreshed, checkedAt: Date.now() }, update: () => void (dynamicModels = refreshed) });
				}
			: undefined,
		filterModels: input.filterModels,
		filterAllModels: input.filterAllModels,
		stream: (model, context, options) => dispatch(model, (implementation) => implementation.stream(model, context, options)),
		streamSimple: (model, context, options) => dispatch(model, (implementation) => implementation.streamSimple(model, context, options)),
	};
	if (streams.some((entry) => entry.fetchDeferred !== undefined)) {
		provider.fetchDeferred = (model, handle, options) =>
			lazyStream(model, async () => {
				const implementation = apiFor(model);
				if (!implementation?.fetchDeferred) throw new ModelsError("provider", `Provider ${input.id} does not support deferred responses for "${model.api}"`);
				return implementation.fetchDeferred(model, handle, options);
			});
	}
	if (streams.some((entry) => entry.cancelDeferred !== undefined)) {
		provider.cancelDeferred = async (model, handle, options) => {
			const implementation = apiFor(model);
			if (!implementation?.cancelDeferred) throw new ModelsError("provider", `Provider ${input.id} cannot cancel deferred responses for "${model.api}"`);
			await implementation.cancelDeferred(model, handle, options);
		};
	}
	if (images && imageImplementations.length > 0) {
		provider.generateImages = async (model, context, options) => {
			const implementation = images[model.api];
			if (!implementation) throw new ModelsError("provider", `Provider ${input.id} has no image generation implementation for "${model.api}"`);
			return implementation.generateImages(model, context, options);
		};
	}
	if (classifiers && classifierImplementations.length > 0) {
		provider.classify = async (model, context, options) => {
			const implementation = classifiers[model.api];
			if (!implementation) throw new ModelsError("provider", `Provider ${input.id} has no classifier implementation for "${model.api}"`);
			return implementation.classify(model, context, options);
		};
	}
	return provider;
}
export function declarationsEqual() {
	return unavailable("declarationsEqual");
}
export function defaultProviderAuthContext() {
	return unavailable("defaultProviderAuthContext");
}
export function envApiKeyAuth(name, envVars) {
	return {
		name,
		login: async (interaction) => {
			interaction.signal.throwIfAborted();
			const key = await interaction.prompt({ type: "secret", message: `Enter ${name}` });
			interaction.signal.throwIfAborted();
			return { type: "api_key", key };
		},
		resolve: async ({ ctx, credential, signal }) => {
			signal.throwIfAborted();
			if (credential?.key) return { auth: { apiKey: credential.key }, env: credential.env, source: "stored credential" };
			for (const envVar of envVars) {
				const value = await ctx.env(envVar);
				signal.throwIfAborted();
				if (value) return { auth: { apiKey: value }, source: envVar };
			}
			return undefined;
		},
	};
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
	return hostApi;
}
export function googleVertexApi() {
	return hostApi;
}
const HOST_APIS = ["anthropic-messages", "openai-completions", "openai-responses", "azure-openai-responses", "openai-codex-responses", "google-generative-ai", "mistral-conversations"];
export function hasApi(api) {
	return HOST_APIS.includes(api);
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
function setupErrorMessage(model, error) {
	return {
		role: "assistant",
		content: [],
		api: model.api,
		provider: model.provider,
		model: model.id,
		usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
		stopReason: "error",
		errorMessage: error instanceof Error ? error.message : String(error),
		timestamp: Date.now(),
	};
}
export function lazyStream(model, setup) {
	const outer = new AssistantMessageEventStream();
	setup()
		.then(async (inner) => {
			for await (const event of inner) outer.push(event);
			outer.end(typeof inner.result === "function" ? await inner.result() : undefined);
		})
		.catch((error) => {
			const message = setupErrorMessage(model, error);
			outer.push({ type: "error", reason: "error", error: message });
			outer.end(message);
		});
	return outer;
}
export function lazyApi(load, capabilities) {
	const api = {
		stream: (model, context, options) => lazyStream(model, async () => (await load()).stream(model, context, options)),
		streamSimple: (model, context, options) => lazyStream(model, async () => (await load()).streamSimple(model, context, options)),
	};
	if (capabilities?.fetchDeferred) {
		api.fetchDeferred = (model, handle, options) =>
			lazyStream(model, async () => {
				const implementation = await load();
				if (!implementation.fetchDeferred) throw new Error("API does not support deferred responses");
				return implementation.fetchDeferred(model, handle, options);
			});
	}
	if (capabilities?.cancelDeferred) {
		api.cancelDeferred = async (model, handle, options) => {
			const implementation = await load();
			if (!implementation.cancelDeferred) throw new Error("API cannot cancel deferred responses");
			await implementation.cancelDeferred(model, handle, options);
		};
	}
	return api;
}
export function lazyOAuth(input) {
	let promise;
	const loaded = () => (promise ??= input.load());
	return {
		name: input.name,
		isSubscription: input.isSubscription,
		loginLabel: input.loginLabel,
		login: async (interaction, options) => (await loaded()).login(interaction, options),
		refresh: async (credential, signal) => (await loaded()).refresh(credential, signal),
		toAuth: async (credential) => (await loaded()).toAuth(credential),
	};
}
export function mistralConversationsApi() {
	return hostApi;
}
export function normalizeContext() {
	return unavailable("normalizeContext");
}
export function openAICodexResponsesApi() {
	return hostApi;
}
export function openAICompletionsApi() {
	return hostApi;
}
export function openAIResponsesApi() {
	return hostApi;
}
export function piMessagesApi() {
	return hostApi;
}
export function reduceAssistantMessageFrames() {
	return unavailable("reduceAssistantMessageFrames");
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
export const streamAnthropic = hostStream;
export const streamAzureOpenAIResponses = hostStream;
export const streamGoogle = hostStream;
export const streamGoogleVertex = hostStream;
export const streamMistral = hostStream;
export const streamOpenAICodexResponses = hostStream;
export const streamOpenAICompletions = hostStream;
export const streamOpenAIResponses = hostStream;
export const streamSimpleAnthropic = hostStream;
export const streamSimpleAzureOpenAIResponses = hostStream;
export const streamSimpleGoogle = hostStream;
export const streamSimpleGoogleVertex = hostStream;
export const streamSimpleMistral = hostStream;
export const streamSimpleOpenAICodexResponses = hostStream;
export const streamSimpleOpenAICompletions = hostStream;
export const streamSimpleOpenAIResponses = hostStream;
export function toToolDeclaration() {
	return unavailable("toToolDeclaration");
}
export function withoutInitialSystemMessage() {
	return unavailable("withoutInitialSystemMessage");
}

// ----- subpath modules ------------------------------------------------------------------------------
// pi-ai's subpaths (`/providers/*`, `/api/*`, `/utils/*`, `/models`, `/compat`,
// `/oauth`) resolve to this module too. The catalog reads yapi's built-in
// catalog, which has chat models only.
const stub = (name) =>
	function () {
		return unavailable(name);
	};
const stubClass = (name) =>
	class {
		constructor() {
			unavailable(name);
		}
	};
/** A provider's generated `<PROVIDER>_MODELS` table, read on first use. */
function catalog(provider) {
	let models;
	const load = () => (models ??= Object.fromEntries(getModels(provider).map((model) => [model.id, model])));
	return new Proxy(
		{},
		{
			get: (_target, key) => load()[key],
			has: (_target, key) => key in load(),
			ownKeys: () => Reflect.ownKeys(load()),
			getOwnPropertyDescriptor: (_target, key) => {
				const descriptor = Reflect.getOwnPropertyDescriptor(load(), key);
				if (descriptor) descriptor.configurable = true;
				return descriptor;
			},
		},
	);
}
export const getBuiltinModels = (provider) => getModels(provider);
export const getBuiltinModel = (provider, id) => getModel(provider, id);
export const getBuiltinProviders = () => getProviders();
export const getBuiltinImageModels = () => [];
export const getBuiltinImageModel = () => undefined;
export const getBuiltinClassifierModels = () => [];
export const getBuiltinClassifierModel = () => undefined;
export const getAllBuiltinModels = (provider) => getModels(provider);
export const getBuiltinModelDataGeneratedAt = () => undefined;
export function getProviderEnvValue(name, env) {
	return env?.[name] || process.env[name] || undefined;
}
export function sleep(ms, signal) {
	return new Promise((resolve, reject) => {
		signal.throwIfAborted();
		const onAbort = () => {
			clearTimeout(timeout);
			reject(signal.reason);
		};
		const timeout = setTimeout(() => {
			signal.removeEventListener("abort", onAbort);
			resolve();
		}, ms);
		signal.addEventListener("abort", onAbort, { once: true });
	});
}
// Generated from pi-ai 1.0.0's subpath modules: constants as published, the
// rest stubs that throw when called.
export const builtinModels = stub("builtinModels");
export const builtinProviders = stub("builtinProviders");
export const radiusProvider = stub("radiusProvider");
export const amazonBedrockProvider = stub("amazonBedrockProvider");
export const AMAZON_BEDROCK_CLASSIFIER_MODELS = Object.freeze({});
export const AMAZON_BEDROCK_IMAGE_MODELS = Object.freeze({});
export const AMAZON_BEDROCK_MODELS = catalog("amazon-bedrock");
export const antLingProvider = stub("antLingProvider");
export const ANT_LING_CLASSIFIER_MODELS = Object.freeze({});
export const ANT_LING_IMAGE_MODELS = Object.freeze({});
export const ANT_LING_MODELS = catalog("ant-ling");
export const anthropicProvider = stub("anthropicProvider");
export const ANTHROPIC_CLASSIFIER_MODELS = Object.freeze({});
export const ANTHROPIC_IMAGE_MODELS = Object.freeze({});
export const ANTHROPIC_MODELS = catalog("anthropic");
export const azureOpenAIResponsesProvider = stub("azureOpenAIResponsesProvider");
export const AZURE_OPENAI_RESPONSES_CLASSIFIER_MODELS = Object.freeze({});
export const AZURE_OPENAI_RESPONSES_IMAGE_MODELS = Object.freeze({});
export const AZURE_OPENAI_RESPONSES_MODELS = catalog("azure-openai-responses");
export const basetenProvider = stub("basetenProvider");
export const BASETEN_CLASSIFIER_MODELS = Object.freeze({});
export const BASETEN_IMAGE_MODELS = Object.freeze({});
export const BASETEN_MODELS = catalog("baseten");
export const cerebrasProvider = stub("cerebrasProvider");
export const CEREBRAS_CLASSIFIER_MODELS = Object.freeze({});
export const CEREBRAS_IMAGE_MODELS = Object.freeze({});
export const CEREBRAS_MODELS = catalog("cerebras");
export const cloudflareAIGatewayProvider = stub("cloudflareAIGatewayProvider");
export const CLOUDFLARE_AI_GATEWAY_CLASSIFIER_MODELS = Object.freeze({});
export const CLOUDFLARE_AI_GATEWAY_IMAGE_MODELS = Object.freeze({});
export const CLOUDFLARE_AI_GATEWAY_MODELS = catalog("cloudflare-ai-gateway");
export const cloudflareAIGatewayAuth = stub("cloudflareAIGatewayAuth");
export const cloudflareWorkersAIAuth = stub("cloudflareWorkersAIAuth");
export const cloudflareClassifier = stub("cloudflareClassifier");
export const cloudflareStreams = stub("cloudflareStreams");
export const resolveCloudflareModel = stub("resolveCloudflareModel");
export const cloudflareWorkersAIProvider = stub("cloudflareWorkersAIProvider");
export const CLOUDFLARE_WORKERS_AI_CLASSIFIER_MODELS = Object.freeze({});
export const CLOUDFLARE_WORKERS_AI_IMAGE_MODELS = Object.freeze({});
export const CLOUDFLARE_WORKERS_AI_MODELS = catalog("cloudflare-workers-ai");
export const deepseekProvider = stub("deepseekProvider");
export const DEEPSEEK_CLASSIFIER_MODELS = Object.freeze({});
export const DEEPSEEK_IMAGE_MODELS = Object.freeze({});
export const DEEPSEEK_MODELS = catalog("deepseek");
export const fireworksProvider = stub("fireworksProvider");
export const FIREWORKS_CLASSIFIER_MODELS = Object.freeze({});
export const FIREWORKS_IMAGE_MODELS = Object.freeze({});
export const FIREWORKS_MODELS = catalog("fireworks");
export const githubCopilotProvider = stub("githubCopilotProvider");
export const GITHUB_COPILOT_CLASSIFIER_MODELS = Object.freeze({});
export const GITHUB_COPILOT_IMAGE_MODELS = Object.freeze({});
export const GITHUB_COPILOT_MODELS = catalog("github-copilot");
export const googleVertexProvider = stub("googleVertexProvider");
export const GOOGLE_VERTEX_CLASSIFIER_MODELS = Object.freeze({});
export const GOOGLE_VERTEX_IMAGE_MODELS = Object.freeze({});
export const GOOGLE_VERTEX_MODELS = catalog("google-vertex");
export const googleProvider = stub("googleProvider");
export const GOOGLE_CLASSIFIER_MODELS = Object.freeze({});
export const GOOGLE_IMAGE_MODELS = Object.freeze({});
export const GOOGLE_MODELS = catalog("google");
export const groqProvider = stub("groqProvider");
export const GROQ_CLASSIFIER_MODELS = Object.freeze({});
export const GROQ_IMAGE_MODELS = Object.freeze({});
export const GROQ_MODELS = catalog("groq");
export const huggingfaceProvider = stub("huggingfaceProvider");
export const HUGGINGFACE_CLASSIFIER_MODELS = Object.freeze({});
export const HUGGINGFACE_IMAGE_MODELS = Object.freeze({});
export const HUGGINGFACE_MODELS = catalog("huggingface");
export const kimiCodingProvider = stub("kimiCodingProvider");
export const KIMI_CODING_CLASSIFIER_MODELS = Object.freeze({});
export const KIMI_CODING_IMAGE_MODELS = Object.freeze({});
export const KIMI_CODING_MODELS = catalog("kimi-coding");
export const metaProvider = stub("metaProvider");
export const META_CLASSIFIER_MODELS = Object.freeze({});
export const META_IMAGE_MODELS = Object.freeze({});
export const META_MODELS = catalog("meta");
export const minimaxCnProvider = stub("minimaxCnProvider");
export const MINIMAX_CN_CLASSIFIER_MODELS = Object.freeze({});
export const MINIMAX_CN_IMAGE_MODELS = Object.freeze({});
export const MINIMAX_CN_MODELS = catalog("minimax-cn");
export const minimaxProvider = stub("minimaxProvider");
export const MINIMAX_CLASSIFIER_MODELS = Object.freeze({});
export const MINIMAX_IMAGE_MODELS = Object.freeze({});
export const MINIMAX_MODELS = catalog("minimax");
export const mistralProvider = stub("mistralProvider");
export const MISTRAL_CLASSIFIER_MODELS = Object.freeze({});
export const MISTRAL_IMAGE_MODELS = Object.freeze({});
export const MISTRAL_MODELS = catalog("mistral");
export const moonshotaiCnProvider = stub("moonshotaiCnProvider");
export const MOONSHOTAI_CN_CLASSIFIER_MODELS = Object.freeze({});
export const MOONSHOTAI_CN_IMAGE_MODELS = Object.freeze({});
export const MOONSHOTAI_CN_MODELS = catalog("moonshotai-cn");
export const moonshotaiProvider = stub("moonshotaiProvider");
export const MOONSHOTAI_CLASSIFIER_MODELS = Object.freeze({});
export const MOONSHOTAI_IMAGE_MODELS = Object.freeze({});
export const MOONSHOTAI_MODELS = catalog("moonshotai");
export const nvidiaProvider = stub("nvidiaProvider");
export const NVIDIA_CLASSIFIER_MODELS = Object.freeze({});
export const NVIDIA_IMAGE_MODELS = Object.freeze({});
export const NVIDIA_MODELS = catalog("nvidia");
export const openaiCodexProvider = stub("openaiCodexProvider");
export const OPENAI_CODEX_CLASSIFIER_MODELS = Object.freeze({});
export const OPENAI_CODEX_IMAGE_MODELS = Object.freeze({});
export const OPENAI_CODEX_MODELS = catalog("openai-codex");
export const openaiProvider = stub("openaiProvider");
export const OPENAI_CLASSIFIER_MODELS = Object.freeze({});
export const OPENAI_IMAGE_MODELS = Object.freeze({});
export const OPENAI_MODELS = catalog("openai");
export const opencodeGoProvider = stub("opencodeGoProvider");
export const OPENCODE_GO_CLASSIFIER_MODELS = Object.freeze({});
export const OPENCODE_GO_IMAGE_MODELS = Object.freeze({});
export const OPENCODE_GO_MODELS = catalog("opencode-go");
export const withOpenCodeSessionHeader = stub("withOpenCodeSessionHeader");
export const opencodeProvider = stub("opencodeProvider");
export const OPENCODE_CLASSIFIER_MODELS = Object.freeze({});
export const OPENCODE_IMAGE_MODELS = Object.freeze({});
export const OPENCODE_MODELS = catalog("opencode");
export const openrouterProvider = stub("openrouterProvider");
export const OPENROUTER_CLASSIFIER_MODELS = Object.freeze({});
export const OPENROUTER_IMAGE_MODELS = Object.freeze({});
export const OPENROUTER_MODELS = catalog("openrouter");
export const qwenTokenPlanCnProvider = stub("qwenTokenPlanCnProvider");
export const QWEN_TOKEN_PLAN_CN_CLASSIFIER_MODELS = Object.freeze({});
export const QWEN_TOKEN_PLAN_CN_IMAGE_MODELS = Object.freeze({});
export const QWEN_TOKEN_PLAN_CN_MODELS = catalog("qwen-token-plan-cn");
export const qwenTokenPlanIndividualProvider = stub("qwenTokenPlanIndividualProvider");
export const QWEN_TOKEN_PLAN_INDIVIDUAL_CLASSIFIER_MODELS = Object.freeze({});
export const QWEN_TOKEN_PLAN_INDIVIDUAL_IMAGE_MODELS = Object.freeze({});
export const QWEN_TOKEN_PLAN_INDIVIDUAL_MODELS = catalog("qwen-token-plan-individual");
export const qwenTokenPlanProvider = stub("qwenTokenPlanProvider");
export const QWEN_TOKEN_PLAN_CLASSIFIER_MODELS = Object.freeze({});
export const QWEN_TOKEN_PLAN_IMAGE_MODELS = Object.freeze({});
export const QWEN_TOKEN_PLAN_MODELS = catalog("qwen-token-plan");
export const DEFAULT_RADIUS_GATEWAY = "https://radius.pi.dev";
export const getRadiusCredentialConfig = stub("getRadiusCredentialConfig");
export const getRadiusModels = stub("getRadiusModels");
export const getRadiusModelsFromConfig = stub("getRadiusModelsFromConfig");
export const loadRadiusGatewayConfig = stub("loadRadiusGatewayConfig");
export const normalizeRadiusGatewayUrl = stub("normalizeRadiusGatewayUrl");
export const RADIUS_CLASSIFIER_MODELS = Object.freeze({});
export const RADIUS_IMAGE_MODELS = Object.freeze({});
export const RADIUS_MODELS = catalog("radius");
export const togetherProvider = stub("togetherProvider");
export const TOGETHER_CLASSIFIER_MODELS = Object.freeze({});
export const TOGETHER_IMAGE_MODELS = Object.freeze({});
export const TOGETHER_MODELS = catalog("together");
export const typesafeProvider = stub("typesafeProvider");
export const TYPESAFE_CLASSIFIER_MODELS = Object.freeze({});
export const TYPESAFE_IMAGE_MODELS = Object.freeze({});
export const TYPESAFE_MODELS = catalog("typesafe");
export const vercelAIGatewayProvider = stub("vercelAIGatewayProvider");
export const VERCEL_AI_GATEWAY_CLASSIFIER_MODELS = Object.freeze({});
export const VERCEL_AI_GATEWAY_IMAGE_MODELS = Object.freeze({});
export const VERCEL_AI_GATEWAY_MODELS = catalog("vercel-ai-gateway");
export const xaiProvider = stub("xaiProvider");
export const XAI_CLASSIFIER_MODELS = Object.freeze({});
export const XAI_IMAGE_MODELS = Object.freeze({});
export const XAI_MODELS = catalog("xai");
export const xiaomiTokenPlanAmsProvider = stub("xiaomiTokenPlanAmsProvider");
export const XIAOMI_TOKEN_PLAN_AMS_CLASSIFIER_MODELS = Object.freeze({});
export const XIAOMI_TOKEN_PLAN_AMS_IMAGE_MODELS = Object.freeze({});
export const XIAOMI_TOKEN_PLAN_AMS_MODELS = catalog("xiaomi-token-plan-ams");
export const xiaomiTokenPlanCnProvider = stub("xiaomiTokenPlanCnProvider");
export const XIAOMI_TOKEN_PLAN_CN_CLASSIFIER_MODELS = Object.freeze({});
export const XIAOMI_TOKEN_PLAN_CN_IMAGE_MODELS = Object.freeze({});
export const XIAOMI_TOKEN_PLAN_CN_MODELS = catalog("xiaomi-token-plan-cn");
export const xiaomiTokenPlanSgpProvider = stub("xiaomiTokenPlanSgpProvider");
export const XIAOMI_TOKEN_PLAN_SGP_CLASSIFIER_MODELS = Object.freeze({});
export const XIAOMI_TOKEN_PLAN_SGP_IMAGE_MODELS = Object.freeze({});
export const XIAOMI_TOKEN_PLAN_SGP_MODELS = catalog("xiaomi-token-plan-sgp");
export const xiaomiProvider = stub("xiaomiProvider");
export const XIAOMI_CLASSIFIER_MODELS = Object.freeze({});
export const XIAOMI_IMAGE_MODELS = Object.freeze({});
export const XIAOMI_MODELS = catalog("xiaomi");
export const zaiCodingCnProvider = stub("zaiCodingCnProvider");
export const ZAI_CODING_CN_CLASSIFIER_MODELS = Object.freeze({});
export const ZAI_CODING_CN_IMAGE_MODELS = Object.freeze({});
export const ZAI_CODING_CN_MODELS = catalog("zai-coding-cn");
export const zaiProvider = stub("zaiProvider");
export const ZAI_CLASSIFIER_MODELS = Object.freeze({});
export const ZAI_IMAGE_MODELS = Object.freeze({});
export const ZAI_MODELS = catalog("zai");
export const CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL = "cloudflare-gateway-binding";
export const createAiBindingFetch = stub("createAiBindingFetch");
export const classify = stub("classify");
export const cloudflareWorkersAISystemOneApi = stub("cloudflareWorkersAISystemOneApi");
export const CLOUDFLARE_AI_GATEWAY_ANTHROPIC_BASE_URL = "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/anthropic";
export const CLOUDFLARE_AI_GATEWAY_COMPAT_BASE_URL = "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat";
export const CLOUDFLARE_AI_GATEWAY_OPENAI_BASE_URL = "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai";
export const CLOUDFLARE_WORKERS_AI_BASE_URL = "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1";
export const CLOUDFLARE_WORKERS_AI_REST_BASE_URL = "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai";
export const appendGrammarToolInputJsonDelta = stub("appendGrammarToolInputJsonDelta");
export const createGrammarToolInputProperties = stub("createGrammarToolInputProperties");
export const getGrammarToolInput = stub("getGrammarToolInput");
export const getJsonSchemaToolParameters = stub("getJsonSchemaToolParameters");
export const makeStrictJsonSchema = stub("makeStrictJsonSchema");
export const resolveGrammarConstrainedSampling = stub("resolveGrammarConstrainedSampling");
export const resolveJsonSchemaStrictSampling = stub("resolveJsonSchemaStrictSampling");
export const buildCopilotDynamicHeaders = stub("buildCopilotDynamicHeaders");
export const hasCopilotVisionInput = stub("hasCopilotVisionInput");
export const inferCopilotInitiator = stub("inferCopilotInitiator");
export const convertMessages = stub("convertMessages");
export const convertTools = stub("convertTools");
export const getDisabledGoogleThinkingConfig = stub("getDisabledGoogleThinkingConfig");
export const isThinkingPart = stub("isThinkingPart");
export const mapStopReason = stub("mapStopReason");
export const mapStopReasonString = stub("mapStopReasonString");
export const mapToolChoice = stub("mapToolChoice");
export const requiresToolCallId = stub("requiresToolCallId");
export const resolveGoogleFunctionCallingMode = stub("resolveGoogleFunctionCallingMode");
export const resolveGoogleThinkingLevel = stub("resolveGoogleThinkingLevel");
export const retainThoughtSignature = stub("retainThoughtSignature");
export const retryGoogleRequest = stub("retryGoogleRequest");
export const supportsGoogleStrictToolSampling = stub("supportsGoogleStrictToolSampling");
export const toGoogleSdkThinkingLevel = stub("toGoogleSdkThinkingLevel");
export const toGoogleThinkingLevel = stub("toGoogleThinkingLevel");
export const usesGoogleThinkingLevel = stub("usesGoogleThinkingLevel");
export const answerFromProbabilities = stub("answerFromProbabilities");
export const labelProbabilities = stub("labelProbabilities");
export const llamaServerRoot = stub("llamaServerRoot");
export const peakConfidence = stub("peakConfidence");
export const renderQuestion = stub("renderQuestion");
export const llamaCppClassifyApi = stub("llamaCppClassifyApi");
export const closeOpenAICodexWebSocketSessions = stub("closeOpenAICodexWebSocketSessions");
export const getOpenAICodexWebSocketDebugStats = stub("getOpenAICodexWebSocketDebugStats");
export const resetOpenAICodexWebSocketDebugStats = stub("resetOpenAICodexWebSocketDebugStats");
export const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH = 64;
export const clampOpenAIPromptCacheKey = stub("clampOpenAIPromptCacheKey");
export const convertResponsesMessages = stub("convertResponsesMessages");
export const convertResponsesTools = stub("convertResponsesTools");
export const processResponsesStream = stub("processResponsesStream");
export const openrouterImagesApi = stub("openrouterImagesApi");
export const PiMessagesResponseError = stubClass("PiMessagesResponseError");
export const DEFAULT_THINKING_BUDGETS = {"minimal":1024,"low":2048,"medium":8192,"high":16384};
export const MIN_ANSWER_TOKENS = 1024;
export const adjustMaxTokensForThinking = stub("adjustMaxTokensForThinking");
export const buildBaseOptions = stub("buildBaseOptions");
export const clampMaxTokensToContext = stub("clampMaxTokensToContext");
export const clampReasoning = stub("clampReasoning");
export const clampThinkingBudgetToAnswerRoom = stub("clampThinkingBudgetToAnswerRoom");
export const thinkingBudgetForLevel = stub("thinkingBudgetForLevel");
export const classifySystemOne = stub("classifySystemOne");
export const isRecord = stub("isRecord");
export const transformMessages = stub("transformMessages");
export const typesafeSystemOneApi = stub("typesafeSystemOneApi");
export const combineAbortSignals = stub("combineAbortSignals");
export const operationSignal = stub("operationSignal");
export const raceWithAbortSignal = stub("raceWithAbortSignal");
export const MAX_PROVIDER_ERROR_BODY_CHARS = 4000;
export const formatProviderError = stub("formatProviderError");
export const normalizeProviderError = stub("normalizeProviderError");
export const safeJsonStringify = stub("safeJsonStringify");
export const truncateErrorText = stub("truncateErrorText");
export const calculateContextTokens = stub("calculateContextTokens");
export const estimateContextTokens = stub("estimateContextTokens");
export const estimateMessageTokens = stub("estimateMessageTokens");
export const estimateTextAndImageContentTokens = stub("estimateTextAndImageContentTokens");
export const estimateTextTokens = stub("estimateTextTokens");
export const shortHash = stub("shortHash");
export const headersToRecord = stub("headersToRecord");
export const providerHeadersToRecord = stub("providerHeadersToRecord");
export const assertChatModel = stub("assertChatModel");
export const assertClassifierModel = stub("assertClassifierModel");
export const assertImageModel = stub("assertImageModel");
export const classifierErrorResult = stub("classifierErrorResult");
export const imageErrorResult = stub("imageErrorResult");
export const UNSUPPORTED_PROXY_PROTOCOL_MESSAGE = "Unsupported proxy protocol. SOCKS and PAC proxy URLs are not supported; use an HTTP or HTTPS proxy URL.";
export const resolveHttpProxyUrlForTarget = stub("resolveHttpProxyUrlForTarget");
export const oauthErrorHtml = stub("oauthErrorHtml");
export const oauthSuccessHtml = stub("oauthSuccessHtml");
export const getPiUserAgent = stub("getPiUserAgent");
export const retryProviderRequest = stub("retryProviderRequest");
export const sanitizeSurrogates = stub("sanitizeSurrogates");
