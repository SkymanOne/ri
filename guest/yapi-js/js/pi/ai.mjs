// `@earendil-works/pi-ai` and its subpaths for extensions in yapi. Helpers are
// ports of pi-ai 1.0.0 (MIT, Copyright (c) Mario Zechner); completions run on
// yapi's providers through the host. Stubs cover the rest of pi-ai's exports so
// every import links; a stub throws when called.
import { Type } from "typebox";
import { Value } from "typebox/value";

const yapi = globalThis.__yapi;

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
/**
 * Streams with yapi's implementation of `api`, or of the model's API. The
 * host sends the events that have arrived each time it is asked; the live
 * message is rebuilt here from them.
 */
function hostStream(model, context, options, api) {
	const stream = new AssistantMessageEventStream();
	const { signal, onPayload, onResponse, onProviderStreamEvent, ...rest } = options ?? {};
	(async () => {
		const id = await yapi.op("ai.stream", { api, model, context: normalizeContext(context), options: rest });
		const abort = () => yapi.request("ai.abort", { id });
		if (signal?.aborted) abort();
		else signal?.addEventListener("abort", abort, { once: true });
		try {
			let partial;
			const arguments_ = [];
			for (;;) {
				const batch = await yapi.op("ai.next", { id });
				if (batch.length === 0) return;
				for (const wire of batch) {
					if (wire.type === "start") {
						partial = wire.message;
						stream.push({ type: "start", partial });
					} else if (wire.type === "done") stream.push({ type: "done", reason: wire.message.stopReason, message: wire.message });
					else if (wire.type === "error") stream.push({ type: "error", reason: wire.message.stopReason, error: wire.message });
					else {
						const { id: callId, toolName, ...event } = wire.event;
						const index = event.contentIndex;
						const content = partial.content;
						switch (event.type) {
							case "text_start":
								content[index] = { type: "text", text: "" };
								break;
							case "text_delta":
								content[index].text += event.delta;
								break;
							case "text_end":
								content[index].text = event.content;
								break;
							case "thinking_start":
								content[index] = { type: "thinking", thinking: "" };
								break;
							case "thinking_delta":
								content[index].thinking += event.delta;
								break;
							case "thinking_end":
								content[index].thinking = event.content;
								break;
							case "toolcall_start":
								content[index] = { type: "toolCall", id: callId, name: toolName, arguments: {} };
								arguments_[index] = "";
								break;
							case "toolcall_delta":
								arguments_[index] += event.delta;
								content[index].arguments = parseStreamingJson(arguments_[index]);
								break;
							case "toolcall_end":
								content[index] = event.toolCall;
								break;
						}
						partial.usage = wire.usage;
						stream.push({ ...event, partial });
					}
				}
			}
		} finally {
			signal?.removeEventListener("abort", abort);
		}
	})().catch((error) => stream.push({ type: "error", reason: "error", error: yapi.setupError(model, error) }));
	return stream;
}
/** yapi's implementation of wire API `api`. */
const hostApi = (api) => {
	const run = (model, context, options) => hostStream(model, context, options, api);
	return { stream: run, streamSimple: run };
};

// ----- API providers -----------------------------------------------------------------------------
// pi-ai's registry. A stream an extension registers for an API serves its
// own calls to `stream` and `streamSimple`, and the session's models of that
// API when yapi has no provider for it.
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
// The extension host streams session models through these.
yapi.apiProviders = apiProviders;
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
	return apiProviders.get(api)?.provider ?? (BUILTIN_APIS.includes(api) ? { api, ...hostApi(api) } : undefined);
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

// ----- transcripts ---------------------------------------------------------------------------------
const isSystemMessage = (message) => message.role === "system";

export function getSystemMessageText(message) {
	const parts = [contentText(message.content)];
	for (const text of Object.values(message.sections ?? {})) {
		if (text !== null) parts.push(text);
	}
	return parts.filter((part) => part.length > 0).join("\n\n");
}

export function createInitialSystemMessage(systemPrompt, tools) {
	const hasSystemPrompt = systemPrompt !== undefined && systemPrompt.length > 0;
	const hasTools = tools !== undefined && tools.length > 0;
	if (!hasSystemPrompt && !hasTools) return undefined;
	return { role: "system", content: systemPrompt ?? "", ...(hasTools ? { toolsAdded: tools } : {}), timestamp: 0 };
}

export function normalizeContext(context) {
	const initialMessage = createInitialSystemMessage(context.systemPrompt, context.tools);
	return { messages: initialMessage ? [initialMessage, ...context.messages] : context.messages };
}

export function getInitialSystemMessage(messages) {
	const first = messages[0];
	return first && isSystemMessage(first) ? first : undefined;
}

export function withoutInitialSystemMessage(messages) {
	return getInitialSystemMessage(messages) ? messages.slice(1) : messages;
}

export function getCurrentTools(messages) {
	const tools = new Map();
	for (const message of messages) {
		if (!isSystemMessage(message)) continue;
		for (const tool of message.toolsRemoved ?? []) tools.delete(tool.name);
		for (const tool of message.toolsAdded ?? []) tools.set(tool.name, tool);
	}
	return [...tools.values()];
}

export function getCurrentSystemMessage(messages) {
	const content = [];
	const sections = new Map();
	let timestamp;
	for (const message of messages) {
		if (!isSystemMessage(message)) continue;
		timestamp ??= message.timestamp;
		const text = contentText(message.content);
		if (text.length > 0) content.push(text);
		for (const [name, value] of Object.entries(message.sections ?? {})) {
			if (value === null) sections.delete(name);
			else sections.set(name, value);
		}
	}
	const tools = getCurrentTools(messages);
	if (timestamp === undefined && tools.length === 0) return undefined;
	return {
		role: "system",
		content: content.join("\n\n"),
		...(sections.size > 0 ? { sections: Object.fromEntries(sections) } : {}),
		...(tools.length > 0 ? { toolsAdded: tools } : {}),
		timestamp: timestamp ?? 0,
	};
}

export function getCurrentSystemPrompt(messages) {
	const message = getCurrentSystemMessage(messages);
	return message ? getSystemMessageText(message) : "";
}

export function collapseSystemMessages(context) {
	const head = getCurrentSystemMessage(context.messages);
	const messages = context.messages.filter((message) => message.role !== "system");
	return { messages: head ? [head, ...messages] : messages };
}

export function resolveTranscript(context, supportsMidConvoSystemMessages) {
	return supportsMidConvoSystemMessages ? context : collapseSystemMessages(context);
}

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
export const { AssistantMessageFrameEncoder, InMemoryCredentialStore, InMemoryModelsStore, appendAssistantMessageDiagnostic, cleanupSessionResources,
	createAssistantMessageDiagnostic, createFauxCore, createModels, declarationsEqual, defaultProviderAuthContext, extractDiagnosticError,
	fauxAssistantMessage, fauxProvider, fauxText, fauxThinking, fauxToolCall, findEnvKeys, generateImages, generateImagesOpenRouter, getDeclaredTools,
	getImageModel, getImageModels, getImageProviders, getImagesApiProvider, getOverflowPatterns, getToolStateChanges, hasNonAdditiveToolChanges,
	hasToolRedefinitions, isContextOverflow, isRecoverableLength, isRetryableAssistantError, reduceAssistantMessageFrames,
	registerBuiltInImagesApiProviders, registerFauxProvider, registerImagesApiProvider, registerSessionResourceCleanup, renderSystemMessageUpdate,
	resolveTranscriptTools, retryAssistantCall, retryDelayMs, setBedrockProviderModule, toToolDeclaration } = yapi.stubs("@earendil-works/pi-ai");
export const ANTHROPIC_API_KEY_ENV = "ANTHROPIC_API_KEY";
export const ANTHROPIC_AUTH_TOKEN_ENV = "ANTHROPIC_AUTH_TOKEN";
export const ANTHROPIC_FEDERATION_RULE_ID_ENV = "ANTHROPIC_FEDERATION_RULE_ID";
export const ANTHROPIC_IDENTITY_TOKEN_FILE_ENV = "ANTHROPIC_IDENTITY_TOKEN_FILE";
export const ANTHROPIC_OAUTH_TOKEN_ENV = "ANTHROPIC_OAUTH_TOKEN";
export const ANTHROPIC_ORGANIZATION_ID_ENV = "ANTHROPIC_ORGANIZATION_ID";
export const ANTHROPIC_SERVICE_ACCOUNT_ID_ENV = "ANTHROPIC_SERVICE_ACCOUNT_ID";
export const ANTHROPIC_WORKSPACE_ID_ENV = "ANTHROPIC_WORKSPACE_ID";
export class ModelsError extends Error {
	constructor(kind, message, options) {
		super(message, options);
		this.name = "ModelsError";
		this.kind = kind;
	}
}
export function anthropicMessagesApi() {
	return hostApi("anthropic-messages");
}
export function azureOpenAIResponsesApi() {
	return hostApi("azure-openai-responses");
}
export function bedrockConverseStreamApi() {
	return hostApi("bedrock-converse-stream");
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
export function googleGenerativeAIApi() {
	return hostApi("google-generative-ai");
}
export function googleVertexApi() {
	return hostApi("google-vertex");
}
const HOST_APIS = ["anthropic-messages", "openai-completions", "openai-responses", "azure-openai-responses", "openai-codex-responses", "google-generative-ai", "mistral-conversations"];
export function hasApi(api) {
	return HOST_APIS.includes(api);
}
export function lazyStream(model, setup) {
	const outer = new AssistantMessageEventStream();
	setup()
		.then(async (inner) => {
			for await (const event of inner) outer.push(event);
			outer.end(typeof inner.result === "function" ? await inner.result() : undefined);
		})
		.catch((error) => {
			const message = yapi.setupError(model, error);
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
	return hostApi("mistral-conversations");
}
export function openAICodexResponsesApi() {
	return hostApi("openai-codex-responses");
}
export function openAICompletionsApi() {
	return hostApi("openai-completions");
}
export function openAIResponsesApi() {
	return hostApi("openai-responses");
}
export function piMessagesApi() {
	return hostApi("pi-messages");
}
export const streamAnthropic = hostApi("anthropic-messages").stream;
export const streamAzureOpenAIResponses = hostApi("azure-openai-responses").stream;
export const streamGoogle = hostApi("google-generative-ai").stream;
export const streamGoogleVertex = hostApi("google-vertex").stream;
export const streamMistral = hostApi("mistral-conversations").stream;
export const streamOpenAICodexResponses = hostApi("openai-codex-responses").stream;
export const streamOpenAICompletions = hostApi("openai-completions").stream;
export const streamOpenAIResponses = hostApi("openai-responses").stream;
export const streamSimpleAnthropic = hostApi("anthropic-messages").streamSimple;
export const streamSimpleAzureOpenAIResponses = hostApi("azure-openai-responses").streamSimple;
export const streamSimpleGoogle = hostApi("google-generative-ai").streamSimple;
export const streamSimpleGoogleVertex = hostApi("google-vertex").streamSimple;
export const streamSimpleMistral = hostApi("mistral-conversations").streamSimple;
export const streamSimpleOpenAICodexResponses = hostApi("openai-codex-responses").streamSimple;
export const streamSimpleOpenAICompletions = hostApi("openai-completions").streamSimple;
export const streamSimpleOpenAIResponses = hostApi("openai-responses").streamSimple;

// ----- subpath modules ------------------------------------------------------------------------------
// pi-ai's subpaths (`/providers/*`, `/api/*`, `/utils/*`, `/models`, `/compat`,
// `/oauth`) resolve to this module too. The catalog reads yapi's built-in
// catalog, which has chat models only.
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
export const { builtinModels, builtinProviders, radiusProvider, amazonBedrockProvider, antLingProvider, anthropicProvider,
	azureOpenAIResponsesProvider, basetenProvider, cerebrasProvider, cloudflareAIGatewayProvider, cloudflareAIGatewayAuth, cloudflareWorkersAIAuth,
	cloudflareClassifier, cloudflareStreams, resolveCloudflareModel, cloudflareWorkersAIProvider, deepseekProvider, fireworksProvider,
	githubCopilotProvider, googleVertexProvider, googleProvider, groqProvider, huggingfaceProvider, kimiCodingProvider, metaProvider, minimaxCnProvider,
	minimaxProvider, mistralProvider, moonshotaiCnProvider, moonshotaiProvider, nvidiaProvider, openaiCodexProvider, openaiProvider, opencodeGoProvider,
	withOpenCodeSessionHeader, opencodeProvider, openrouterProvider, qwenTokenPlanCnProvider, qwenTokenPlanIndividualProvider, qwenTokenPlanProvider,
	getRadiusCredentialConfig, getRadiusModels, getRadiusModelsFromConfig, loadRadiusGatewayConfig, normalizeRadiusGatewayUrl, togetherProvider,
	typesafeProvider, vercelAIGatewayProvider, xaiProvider, xiaomiTokenPlanAmsProvider, xiaomiTokenPlanCnProvider, xiaomiTokenPlanSgpProvider,
	xiaomiProvider, zaiCodingCnProvider, zaiProvider, createAiBindingFetch, classify, cloudflareWorkersAISystemOneApi, appendGrammarToolInputJsonDelta,
	createGrammarToolInputProperties, getGrammarToolInput, getJsonSchemaToolParameters, makeStrictJsonSchema, resolveGrammarConstrainedSampling,
	resolveJsonSchemaStrictSampling, buildCopilotDynamicHeaders, hasCopilotVisionInput, inferCopilotInitiator, convertMessages, convertTools,
	getDisabledGoogleThinkingConfig, isThinkingPart, mapStopReason, mapStopReasonString, mapToolChoice, requiresToolCallId,
	resolveGoogleFunctionCallingMode, resolveGoogleThinkingLevel, retainThoughtSignature, retryGoogleRequest, supportsGoogleStrictToolSampling,
	toGoogleSdkThinkingLevel, toGoogleThinkingLevel, usesGoogleThinkingLevel, answerFromProbabilities, labelProbabilities, llamaServerRoot,
	peakConfidence, renderQuestion, llamaCppClassifyApi, closeOpenAICodexWebSocketSessions, getOpenAICodexWebSocketDebugStats,
	resetOpenAICodexWebSocketDebugStats, clampOpenAIPromptCacheKey, convertResponsesMessages, convertResponsesTools, processResponsesStream,
	openrouterImagesApi, PiMessagesResponseError, adjustMaxTokensForThinking, buildBaseOptions, clampMaxTokensToContext, clampReasoning,
	clampThinkingBudgetToAnswerRoom, thinkingBudgetForLevel, classifySystemOne, isRecord, transformMessages, typesafeSystemOneApi, combineAbortSignals,
	operationSignal, raceWithAbortSignal, formatProviderError, normalizeProviderError, safeJsonStringify, truncateErrorText, calculateContextTokens,
	estimateContextTokens, estimateMessageTokens, estimateTextAndImageContentTokens, estimateTextTokens, shortHash, headersToRecord,
	providerHeadersToRecord, assertChatModel, assertClassifierModel, assertImageModel, classifierErrorResult, imageErrorResult,
	resolveHttpProxyUrlForTarget, oauthErrorHtml, oauthSuccessHtml, getPiUserAgent, retryProviderRequest, sanitizeSurrogates } = yapi.stubs("@earendil-works/pi-ai");
export const AMAZON_BEDROCK_CLASSIFIER_MODELS = Object.freeze({});
export const AMAZON_BEDROCK_IMAGE_MODELS = Object.freeze({});
export const AMAZON_BEDROCK_MODELS = catalog("amazon-bedrock");
export const ANT_LING_CLASSIFIER_MODELS = Object.freeze({});
export const ANT_LING_IMAGE_MODELS = Object.freeze({});
export const ANT_LING_MODELS = catalog("ant-ling");
export const ANTHROPIC_CLASSIFIER_MODELS = Object.freeze({});
export const ANTHROPIC_IMAGE_MODELS = Object.freeze({});
export const ANTHROPIC_MODELS = catalog("anthropic");
export const AZURE_OPENAI_RESPONSES_CLASSIFIER_MODELS = Object.freeze({});
export const AZURE_OPENAI_RESPONSES_IMAGE_MODELS = Object.freeze({});
export const AZURE_OPENAI_RESPONSES_MODELS = catalog("azure-openai-responses");
export const BASETEN_CLASSIFIER_MODELS = Object.freeze({});
export const BASETEN_IMAGE_MODELS = Object.freeze({});
export const BASETEN_MODELS = catalog("baseten");
export const CEREBRAS_CLASSIFIER_MODELS = Object.freeze({});
export const CEREBRAS_IMAGE_MODELS = Object.freeze({});
export const CEREBRAS_MODELS = catalog("cerebras");
export const CLOUDFLARE_AI_GATEWAY_CLASSIFIER_MODELS = Object.freeze({});
export const CLOUDFLARE_AI_GATEWAY_IMAGE_MODELS = Object.freeze({});
export const CLOUDFLARE_AI_GATEWAY_MODELS = catalog("cloudflare-ai-gateway");
export const CLOUDFLARE_WORKERS_AI_CLASSIFIER_MODELS = Object.freeze({});
export const CLOUDFLARE_WORKERS_AI_IMAGE_MODELS = Object.freeze({});
export const CLOUDFLARE_WORKERS_AI_MODELS = catalog("cloudflare-workers-ai");
export const DEEPSEEK_CLASSIFIER_MODELS = Object.freeze({});
export const DEEPSEEK_IMAGE_MODELS = Object.freeze({});
export const DEEPSEEK_MODELS = catalog("deepseek");
export const FIREWORKS_CLASSIFIER_MODELS = Object.freeze({});
export const FIREWORKS_IMAGE_MODELS = Object.freeze({});
export const FIREWORKS_MODELS = catalog("fireworks");
export const GITHUB_COPILOT_CLASSIFIER_MODELS = Object.freeze({});
export const GITHUB_COPILOT_IMAGE_MODELS = Object.freeze({});
export const GITHUB_COPILOT_MODELS = catalog("github-copilot");
export const GOOGLE_VERTEX_CLASSIFIER_MODELS = Object.freeze({});
export const GOOGLE_VERTEX_IMAGE_MODELS = Object.freeze({});
export const GOOGLE_VERTEX_MODELS = catalog("google-vertex");
export const GOOGLE_CLASSIFIER_MODELS = Object.freeze({});
export const GOOGLE_IMAGE_MODELS = Object.freeze({});
export const GOOGLE_MODELS = catalog("google");
export const GROQ_CLASSIFIER_MODELS = Object.freeze({});
export const GROQ_IMAGE_MODELS = Object.freeze({});
export const GROQ_MODELS = catalog("groq");
export const HUGGINGFACE_CLASSIFIER_MODELS = Object.freeze({});
export const HUGGINGFACE_IMAGE_MODELS = Object.freeze({});
export const HUGGINGFACE_MODELS = catalog("huggingface");
export const KIMI_CODING_CLASSIFIER_MODELS = Object.freeze({});
export const KIMI_CODING_IMAGE_MODELS = Object.freeze({});
export const KIMI_CODING_MODELS = catalog("kimi-coding");
export const META_CLASSIFIER_MODELS = Object.freeze({});
export const META_IMAGE_MODELS = Object.freeze({});
export const META_MODELS = catalog("meta");
export const MINIMAX_CN_CLASSIFIER_MODELS = Object.freeze({});
export const MINIMAX_CN_IMAGE_MODELS = Object.freeze({});
export const MINIMAX_CN_MODELS = catalog("minimax-cn");
export const MINIMAX_CLASSIFIER_MODELS = Object.freeze({});
export const MINIMAX_IMAGE_MODELS = Object.freeze({});
export const MINIMAX_MODELS = catalog("minimax");
export const MISTRAL_CLASSIFIER_MODELS = Object.freeze({});
export const MISTRAL_IMAGE_MODELS = Object.freeze({});
export const MISTRAL_MODELS = catalog("mistral");
export const MOONSHOTAI_CN_CLASSIFIER_MODELS = Object.freeze({});
export const MOONSHOTAI_CN_IMAGE_MODELS = Object.freeze({});
export const MOONSHOTAI_CN_MODELS = catalog("moonshotai-cn");
export const MOONSHOTAI_CLASSIFIER_MODELS = Object.freeze({});
export const MOONSHOTAI_IMAGE_MODELS = Object.freeze({});
export const MOONSHOTAI_MODELS = catalog("moonshotai");
export const NVIDIA_CLASSIFIER_MODELS = Object.freeze({});
export const NVIDIA_IMAGE_MODELS = Object.freeze({});
export const NVIDIA_MODELS = catalog("nvidia");
export const OPENAI_CODEX_CLASSIFIER_MODELS = Object.freeze({});
export const OPENAI_CODEX_IMAGE_MODELS = Object.freeze({});
export const OPENAI_CODEX_MODELS = catalog("openai-codex");
export const OPENAI_CLASSIFIER_MODELS = Object.freeze({});
export const OPENAI_IMAGE_MODELS = Object.freeze({});
export const OPENAI_MODELS = catalog("openai");
export const OPENCODE_GO_CLASSIFIER_MODELS = Object.freeze({});
export const OPENCODE_GO_IMAGE_MODELS = Object.freeze({});
export const OPENCODE_GO_MODELS = catalog("opencode-go");
export const OPENCODE_CLASSIFIER_MODELS = Object.freeze({});
export const OPENCODE_IMAGE_MODELS = Object.freeze({});
export const OPENCODE_MODELS = catalog("opencode");
export const OPENROUTER_CLASSIFIER_MODELS = Object.freeze({});
export const OPENROUTER_IMAGE_MODELS = Object.freeze({});
export const OPENROUTER_MODELS = catalog("openrouter");
export const QWEN_TOKEN_PLAN_CN_CLASSIFIER_MODELS = Object.freeze({});
export const QWEN_TOKEN_PLAN_CN_IMAGE_MODELS = Object.freeze({});
export const QWEN_TOKEN_PLAN_CN_MODELS = catalog("qwen-token-plan-cn");
export const QWEN_TOKEN_PLAN_INDIVIDUAL_CLASSIFIER_MODELS = Object.freeze({});
export const QWEN_TOKEN_PLAN_INDIVIDUAL_IMAGE_MODELS = Object.freeze({});
export const QWEN_TOKEN_PLAN_INDIVIDUAL_MODELS = catalog("qwen-token-plan-individual");
export const QWEN_TOKEN_PLAN_CLASSIFIER_MODELS = Object.freeze({});
export const QWEN_TOKEN_PLAN_IMAGE_MODELS = Object.freeze({});
export const QWEN_TOKEN_PLAN_MODELS = catalog("qwen-token-plan");
export const DEFAULT_RADIUS_GATEWAY = "https://radius.pi.dev";
export const RADIUS_CLASSIFIER_MODELS = Object.freeze({});
export const RADIUS_IMAGE_MODELS = Object.freeze({});
export const RADIUS_MODELS = catalog("radius");
export const TOGETHER_CLASSIFIER_MODELS = Object.freeze({});
export const TOGETHER_IMAGE_MODELS = Object.freeze({});
export const TOGETHER_MODELS = catalog("together");
export const TYPESAFE_CLASSIFIER_MODELS = Object.freeze({});
export const TYPESAFE_IMAGE_MODELS = Object.freeze({});
export const TYPESAFE_MODELS = catalog("typesafe");
export const VERCEL_AI_GATEWAY_CLASSIFIER_MODELS = Object.freeze({});
export const VERCEL_AI_GATEWAY_IMAGE_MODELS = Object.freeze({});
export const VERCEL_AI_GATEWAY_MODELS = catalog("vercel-ai-gateway");
export const XAI_CLASSIFIER_MODELS = Object.freeze({});
export const XAI_IMAGE_MODELS = Object.freeze({});
export const XAI_MODELS = catalog("xai");
export const XIAOMI_TOKEN_PLAN_AMS_CLASSIFIER_MODELS = Object.freeze({});
export const XIAOMI_TOKEN_PLAN_AMS_IMAGE_MODELS = Object.freeze({});
export const XIAOMI_TOKEN_PLAN_AMS_MODELS = catalog("xiaomi-token-plan-ams");
export const XIAOMI_TOKEN_PLAN_CN_CLASSIFIER_MODELS = Object.freeze({});
export const XIAOMI_TOKEN_PLAN_CN_IMAGE_MODELS = Object.freeze({});
export const XIAOMI_TOKEN_PLAN_CN_MODELS = catalog("xiaomi-token-plan-cn");
export const XIAOMI_TOKEN_PLAN_SGP_CLASSIFIER_MODELS = Object.freeze({});
export const XIAOMI_TOKEN_PLAN_SGP_IMAGE_MODELS = Object.freeze({});
export const XIAOMI_TOKEN_PLAN_SGP_MODELS = catalog("xiaomi-token-plan-sgp");
export const XIAOMI_CLASSIFIER_MODELS = Object.freeze({});
export const XIAOMI_IMAGE_MODELS = Object.freeze({});
export const XIAOMI_MODELS = catalog("xiaomi");
export const ZAI_CODING_CN_CLASSIFIER_MODELS = Object.freeze({});
export const ZAI_CODING_CN_IMAGE_MODELS = Object.freeze({});
export const ZAI_CODING_CN_MODELS = catalog("zai-coding-cn");
export const ZAI_CLASSIFIER_MODELS = Object.freeze({});
export const ZAI_IMAGE_MODELS = Object.freeze({});
export const ZAI_MODELS = catalog("zai");
export const CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL = "cloudflare-gateway-binding";
export const CLOUDFLARE_AI_GATEWAY_ANTHROPIC_BASE_URL = "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/anthropic";
export const CLOUDFLARE_AI_GATEWAY_COMPAT_BASE_URL = "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat";
export const CLOUDFLARE_AI_GATEWAY_OPENAI_BASE_URL = "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai";
export const CLOUDFLARE_WORKERS_AI_BASE_URL = "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1";
export const CLOUDFLARE_WORKERS_AI_REST_BASE_URL = "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai";
export const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH = 64;
export const DEFAULT_THINKING_BUDGETS = {"minimal":1024,"low":2048,"medium":8192,"high":16384};
export const MIN_ANSWER_TOKENS = 1024;
export const MAX_PROVIDER_ERROR_BODY_CHARS = 4000;
export const UNSUPPORTED_PROXY_PROTOCOL_MESSAGE = "Unsupported proxy protocol. SOCKS and PAC proxy URLs are not supported; use an HTTP or HTTPS proxy URL.";
