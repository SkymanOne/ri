// Writes ../models-api/cases.json: the requests pi-ai makes and the results it
// returns for its pi-messages, classifier and image APIs. fetch is stubbed, so
// no request leaves the process. Usage: node models-api.mjs   (Node >= 22.19)

import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "node_modules/@earendil-works/pi-coding-agent/node_modules/@earendil-works/pi-ai/dist");
const piMessages = await import(join(root, "api/pi-messages.js"));
const typesafe = await import(join(root, "api/typesafe-system-one.js"));
const cloudflare = await import(join(root, "api/cloudflare-workers-ai-system-one.js"));
const llama = await import(join(root, "api/llama-cpp-classify.js"));
const images = await import(join(root, "api/openrouter-images.js"));

// Each case sets `reply`, which answers a request from its URL and JSON body
// with [status, body, statusText]; a string body is sent as an event stream.
let seen = [];
let reply = () => [200, {}];
globalThis.fetch = async (input, init = {}) => {
	const url = typeof input === "string" ? input : (input.url ?? String(input));
	const headers = {};
	new Headers(init.headers ?? input.headers ?? {}).forEach((value, name) => {
		headers[name] = value;
	});
	let body = init.body;
	if (body === undefined && input instanceof Request) body = await input.text();
	seen.push({ url, headers, body });
	const [status, payload, statusText] = reply(url, body ? JSON.parse(body) : undefined);
	const stream = typeof payload === "string";
	return new Response(stream ? payload : JSON.stringify(payload), {
		status,
		statusText: statusText ?? "",
		headers: { "content-type": stream ? "text/event-stream" : "application/json" },
	});
};

const cases = {};
async function run(name, call, answer) {
	seen = [];
	reply = answer;
	const result = await call();
	delete result.timestamp;
	for (const diagnostic of result.diagnostics ?? []) {
		delete diagnostic.timestamp;
		delete diagnostic.details?.timestampMs;
		delete diagnostic.error?.stack;
	}
	cases[name] = { requests: seen, result };
}

// pi-messages: a transcript as pi's agent loop sends it.
const piModel = {
	id: "balanced",
	name: "Balanced",
	api: "pi-messages",
	provider: "radius",
	baseUrl: "https://gw.example/v1",
	reasoning: true,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 200000,
	maxTokens: 32000,
};
const transcript = {
	messages: [
		{
			role: "system",
			content: "Be brief.",
			timestamp: 0,
			toolsAdded: [
				{ name: "read", description: "Read a file", parameters: { type: "object", properties: { path: { type: "string" } } } },
			],
		},
		{ role: "user", content: "hi", timestamp: 1 },
	],
};
const piOptions = {
	apiKey: "tok",
	sessionId: "s1",
	maxTokens: 100,
	reasoning: "low",
	cacheRetention: "long",
	headers: { "x-extra": "1" },
};
const events = [
	{ type: "start" },
	{ type: "thinking_start", contentIndex: 0 },
	{ type: "thinking_delta", contentIndex: 0, delta: "Let me" },
	{ type: "thinking_end", contentIndex: 0, content: "Let me look.", contentSignature: "sig-1" },
	{ type: "text_start", contentIndex: 1 },
	{ type: "text_delta", contentIndex: 1, delta: "Reading" },
	{ type: "text_end", contentIndex: 1, content: "Reading it." },
	{ type: "toolcall_start", contentIndex: 2, id: "call_1", toolName: "read" },
	{ type: "toolcall_delta", contentIndex: 2, delta: '{"path":' },
	{ type: "toolcall_delta", contentIndex: 2, delta: '"a.txt"}' },
	{
		type: "toolcall_end",
		contentIndex: 2,
		toolCall: { type: "toolCall", id: "call_1", name: "read", arguments: { path: "a.txt" } },
	},
	{
		type: "done",
		reason: "toolUse",
		usage: {
			input: 10,
			output: 5,
			cacheRead: 2,
			cacheWrite: 0,
			totalTokens: 17,
			cost: { input: 0.1, output: 0.2, cacheRead: 0, cacheWrite: 0, total: 0.3 },
		},
		responseId: "resp_1",
		providerThinkingLevel: "low",
	},
];
const sse = events.map((event) => `data: ${JSON.stringify(event)}\n\n`).join("");
await run("pi_messages", () => piMessages.stream(piModel, transcript, piOptions).result(), () => [200, sse]);
await run(
	"pi_messages_http",
	() => piMessages.stream(piModel, transcript, piOptions).result(),
	() => [429, { error: { message: "slow down", code: "rate_limited" } }, "Too Many Requests"],
);

// Classifiers.
const context = {
	state: { text: "hi" },
	questions: {
		tone: { type: "choice", instructions: "What tone?", criteria: { warm: "Friendly", cold: "" } },
		risk: { type: "score", instructions: "How risky?", criteria: ["none", "some", "high"] },
		spam: { type: "bool", instructions: "Is it spam?", criteria: { true: "Unwanted", false: "" } },
	},
};
const cost = { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 };
const answers = {
	tone: { type: "choice", choice: "warm", probabilities: { warm: 0.9, cold: 0.1 }, confidence: 0.8 },
	risk: { type: "score", score: 0.4, confidence: 0.6 },
	spam: { type: "noul", noul: 0.05 },
};
const typesafeModel = {
	type: "classifier",
	id: "jev-latest",
	name: "Jev",
	api: "typesafe-system-one",
	provider: "typesafe",
	baseUrl: "https://api.typesafe.ai/v1/",
	input: ["text"],
	cost,
	contextWindow: 64000,
	headers: { "x-model": "1" },
};
await run(
	"typesafe",
	() => typesafe.classify(typesafeModel, context, { apiKey: "k", headers: { "X-Opt": "2" } }),
	() => [200, { answers, usage: { input_tokens: 1000, output_tokens: 10 } }],
);
await run(
	"typesafe_missing",
	() => typesafe.classify(typesafeModel, context, { apiKey: "k" }),
	() => [200, { answers: { tone: answers.tone } }],
);
await run(
	"typesafe_http",
	() => typesafe.classify(typesafeModel, context, { apiKey: "k" }),
	() => [401, { error: "bad key" }],
);
const cloudflareModel = {
	type: "classifier",
	id: "@cf/typesafe/jev",
	name: "Jev",
	api: "cloudflare-workers-ai-system-one",
	provider: "cloudflare-workers-ai",
	baseUrl: "https://api.cloudflare.com/client/v4/accounts/acct/ai",
	input: ["text"],
	cost,
	contextWindow: 64000,
};
await run(
	"cloudflare",
	() => cloudflare.classify(cloudflareModel, context, { apiKey: "k" }),
	() => [200, { success: true, result: { state: "Completed", result: { answers } } }],
);
await run(
	"cloudflare_failed",
	() => cloudflare.classify(cloudflareModel, context, { apiKey: "k" }),
	() => [200, { success: false, errors: [{ message: "quota" }, { message: "later" }] }],
);
await run(
	"cloudflare_running",
	() => cloudflare.classify(cloudflareModel, context, { apiKey: "k" }),
	() => [200, { success: true, result: { state: "Running" } }],
);

// llama.cpp: a vocabulary where each label is one token after a newline.
const llamaModel = {
	type: "classifier",
	id: "qwen",
	name: "qwen",
	api: "llama-cpp-classify",
	provider: "llama.cpp",
	baseUrl: "http://127.0.0.1:8080/v1",
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 4096,
};
const vocabulary = {
	"\n": [10],
	"\nA": [10, 65],
	"\nB": [10, 66],
	"\nYes": [10, 900],
	"\nNo": [10, 901],
	"\n0": [10, 48],
	"\n1": [10, 49],
	"\n2": [10, 50],
};
const llamaReply = (url, body) => {
	if (url.endsWith("/tokenize")) return [200, { tokens: vocabulary[body.content] ?? [1, 2] }];
	if (url.endsWith("/apply-template")) {
		return [200, { prompt: `<|im_start|>${body.messages[1].content.length}<think>` }];
	}
	const top = [
		[65, -0.2],
		[66, -1.8],
		[900, -3],
		[901, -0.05],
		[48, -1],
		[49, -0.7],
		[50, -2.5],
	].map(([id, logprob]) => ({ id, logprob }));
	return [200, { completion_probabilities: [{ top_logprobs: top }] }];
};
await run("llama", () => llama.classify(llamaModel, context, { apiKey: "local", temperature: 2 }), llamaReply);

// Images.
const imageModel = {
	type: "image",
	id: "google/gemini-2.5-flash-image",
	name: "Nano",
	api: "openrouter-images",
	provider: "openrouter",
	baseUrl: "https://openrouter.ai/api/v1",
	input: ["text", "image"],
	output: ["image", "text"],
	cost: { input: 0.3, output: 2.5, cacheRead: 0.03, cacheWrite: 0.08 },
};
await run(
	"images",
	() =>
		images.generateImages(
			imageModel,
			{
				input: [
					{ type: "text", text: "a cat" },
					{ type: "image", data: "AAA", mimeType: "image/png" },
				],
			},
			{ apiKey: "k" },
		),
	() => [
		200,
		{
			id: "gen-1",
			choices: [
				{
					message: {
						role: "assistant",
						content: "Here",
						images: [{ image_url: { url: "data:image/png;base64,QUJD" } }, { image_url: "https://x/y.png" }],
					},
				},
			],
			usage: {
				prompt_tokens: 100,
				completion_tokens: 1290,
				prompt_tokens_details: { cached_tokens: 20, cache_write_tokens: 5 },
			},
		},
	],
);
await run(
	"images_http",
	() => images.generateImages(imageModel, { input: [{ type: "text", text: "a cat" }] }, { apiKey: "k" }),
	() => [400, { error: { message: "No endpoints", code: 400 } }],
);

mkdirSync(join(here, "../models-api"), { recursive: true });
writeFileSync(join(here, "../models-api/cases.json"), `${JSON.stringify(cases, null, 1)}\n`);
