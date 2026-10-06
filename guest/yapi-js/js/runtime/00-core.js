// Core globals for extensions running in yapi-js: host plumbing, timers,
// console, process, encodings, Buffer, abort signals, URL and fetch.
//
// `__yapi_native` comes from the Rust side: `request(kind, json)` answers at
// once, `start(kind, json)` begins an operation and returns its id, `done` and
// `fail` finish calls, and `fs(op, json)` reaches the filesystem.
(() => {
	"use strict";
	const native = globalThis.__yapi_native;
	const yapi = {};
	globalThis.__yapi = yapi;

	/** Throws Node's `ERR_NOT_SUPPORTED` error with `message`. */
	yapi.unsupported = (message) => {
		const error = new Error(message);
		error.code = "ERR_NOT_SUPPORTED";
		throw error;
	};

	// ----- host plumbing -------------------------------------------------
	const pendingOps = new Map();
	yapi.request = (kind, payload) => {
		const text = native.request(kind, JSON.stringify(payload ?? null));
		return text === "" ? undefined : JSON.parse(text);
	};
	/** Starts a host operation; resolves with its JSON result. */
	yapi.op = (kind, payload) =>
		new Promise((resolve, reject) => {
			const id = native.start(kind, JSON.stringify(payload ?? null));
			pendingOps.set(id, { resolve, reject });
		});
	yapi.resolve = (op, ok, text) => {
		const pending = pendingOps.get(op);
		if (!pending) return;
		pendingOps.delete(op);
		if (ok) pending.resolve(text === "" ? undefined : JSON.parse(text));
		else pending.reject(new Error(text));
	};
	yapi.log = (level, args) => {
		try {
			native.request("log", JSON.stringify({ level, message: args.map(formatValue).join(" ") }));
		} catch {}
	};

	// ----- formatting (console, util.format) --------------------------------
	function formatValue(value, depth = 0) {
		if (typeof value === "string") return value;
		if (value instanceof Error) return value.stack ? `${value.name}: ${value.message}\n${value.stack}` : `${value.name}: ${value.message}`;
		if (typeof value === "function") return `[Function: ${value.name || "anonymous"}]`;
		if (typeof value === "symbol") return value.toString();
		if (typeof value === "bigint") return `${value}n`;
		if (value === undefined) return "undefined";
		if (value === null || typeof value !== "object") return String(value);
		if (depth > 2) return Array.isArray(value) ? "[Array]" : "[Object]";
		try {
			if (Array.isArray(value)) return `[ ${value.map((item) => formatNested(item, depth + 1)).join(", ")} ]`;
			const entries = Object.entries(value).map(([key, item]) => `${key}: ${formatNested(item, depth + 1)}`);
			return entries.length === 0 ? "{}" : `{ ${entries.join(", ")} }`;
		} catch {
			return "[Object]";
		}
	}
	function formatNested(value, depth) {
		return typeof value === "string" ? `'${value}'` : formatValue(value, depth);
	}
	yapi.formatValue = formatValue;
	yapi.format = (format, ...args) => {
		if (typeof format !== "string") return [format, ...args].map((item) => formatValue(item)).join(" ");
		let index = 0;
		const text = format.replace(/%[sdifjoO%]/g, (token) => {
			if (token === "%%") return "%";
			if (index >= args.length) return token;
			const arg = args[index++];
			switch (token) {
				case "%s":
					return typeof arg === "string" ? arg : formatValue(arg);
				case "%d":
				case "%i":
					return String(token === "%i" ? Math.trunc(Number(arg)) : Number(arg));
				case "%f":
					return String(Number(arg));
				case "%j":
					try {
						return JSON.stringify(arg);
					} catch {
						return "[Circular]";
					}
				default:
					return formatValue(arg);
			}
		});
		return [text, ...args.slice(index).map((item) => formatValue(item))].join(" ");
	};

	globalThis.console = {
		log: (...args) => yapi.log("info", [yapi.format(...args)]),
		info: (...args) => yapi.log("info", [yapi.format(...args)]),
		debug: (...args) => yapi.log("debug", [yapi.format(...args)]),
		warn: (...args) => yapi.log("warn", [yapi.format(...args)]),
		error: (...args) => yapi.log("error", [yapi.format(...args)]),
		trace: (...args) => yapi.log("debug", [yapi.format(...args)]),
		dir: (value) => yapi.log("info", [formatValue(value)]),
		table: (value) => yapi.log("info", [formatValue(value)]),
		time() {},
		timeEnd() {},
		timeLog() {},
		assert(condition, ...args) {
			if (!condition) yapi.log("error", ["Assertion failed", ...args]);
		},
		count() {},
		group() {},
		groupEnd() {},
	};

	// ----- timers ------------------------------------------------------------
	let nextTimer = 1;
	const timers = new Map();
	class Timeout {
		constructor(id, repeat) {
			this._id = id;
			this._repeat = repeat;
			this._ref = true;
		}
		ref() {
			this._ref = true;
			return this;
		}
		unref() {
			this._ref = false;
			return this;
		}
		hasRef() {
			return this._ref;
		}
		refresh() {
			return this;
		}
		[Symbol.toPrimitive]() {
			return this._id;
		}
	}
	function schedule(callback, ms, args, repeat) {
		if (typeof callback !== "function") throw new TypeError("The \"callback\" argument must be of type function");
		const id = nextTimer++;
		const handle = new Timeout(id, repeat);
		const delay = Math.max(0, Number(ms) || 0);
		const arm = () => {
			timers.set(id, handle);
			yapi.op("timer", { ms: delay }).then(() => {
				if (timers.get(id) !== handle) return;
				if (!repeat) timers.delete(id);
				try {
					callback(...args);
				} finally {
					if (repeat && timers.get(id) === handle) arm();
				}
			});
		};
		arm();
		return handle;
	}
	function clear(handle) {
		if (handle == null) return;
		const id = typeof handle === "object" ? handle._id : Number(handle);
		timers.delete(id);
	}
	globalThis.setTimeout = (callback, ms, ...args) => schedule(callback, ms, args, false);
	globalThis.setInterval = (callback, ms, ...args) => schedule(callback, ms, args, true);
	globalThis.setImmediate = (callback, ...args) => schedule(callback, 0, args, false);
	globalThis.clearTimeout = clear;
	globalThis.clearInterval = clear;
	globalThis.clearImmediate = clear;
	if (typeof globalThis.queueMicrotask !== "function") {
		globalThis.queueMicrotask = (callback) => Promise.resolve().then(callback);
	}

	// ----- structuredClone ----------------------------------------------------
	if (typeof globalThis.structuredClone !== "function") {
		globalThis.structuredClone = function structuredClone(value, seen = new Map()) {
			if (value === null || typeof value !== "object") return value;
			if (seen.has(value)) return seen.get(value);
			if (value instanceof Date) return new Date(value.getTime());
			if (value instanceof RegExp) return new RegExp(value.source, value.flags);
			if (ArrayBuffer.isView(value)) return value.slice();
			if (value instanceof Map) {
				const out = new Map();
				seen.set(value, out);
				for (const [key, item] of value) out.set(structuredClone(key, seen), structuredClone(item, seen));
				return out;
			}
			if (value instanceof Set) {
				const out = new Set();
				seen.set(value, out);
				for (const item of value) out.add(structuredClone(item, seen));
				return out;
			}
			const out = Array.isArray(value) ? [] : {};
			seen.set(value, out);
			for (const key of Object.keys(value)) out[key] = structuredClone(value[key], seen);
			return out;
		};
	}

	// ----- text encoding ---------------------------------------------------------
	// UTF-8, Latin-1 and base64 run natively. Lone surrogates encode as
	// U+FFFD, as in Node.
	const utf8Encode = (text) => native.utf8Encode(String(text).toWellFormed());
	const utf8Decode = (bytes) => native.utf8Decode(bytes, false)[0];
	const latin1Decode = (bytes) => native.latin1Decode(bytes);
	const base64Encode = (bytes, url = false) => native.base64Encode(bytes, url);
	const base64Decode = (text) => native.base64Decode(String(text));
	const viewBytes = (input) => (input instanceof Uint8Array ? input : ArrayBuffer.isView(input) ? new Uint8Array(input.buffer, input.byteOffset, input.byteLength) : new Uint8Array(input));
	yapi.utf8Encode = utf8Encode;
	yapi.utf8Decode = utf8Decode;
	yapi.base64Encode = base64Encode;
	yapi.base64Decode = base64Decode;
	if (typeof globalThis.TextEncoder !== "function") {
		globalThis.TextEncoder = class TextEncoder {
			get encoding() {
				return "utf-8";
			}
			encode(text = "") {
				return utf8Encode(text);
			}
		};
	}
	if (typeof globalThis.TextDecoder !== "function") {
		const EMPTY = new Uint8Array(0);
		globalThis.TextDecoder = class TextDecoder {
			#pending = EMPTY;
			#started = false;
			constructor(encoding = "utf-8", options = {}) {
				this.encoding = String(encoding).toLowerCase();
				this.ignoreBOM = !!options?.ignoreBOM;
			}
			decode(input, options) {
				let bytes = input === undefined ? EMPTY : viewBytes(input);
				if (this.encoding === "latin1" || this.encoding === "ascii") return latin1Decode(bytes);
				if (this.#pending.length > 0) {
					const joined = new Uint8Array(this.#pending.length + bytes.length);
					joined.set(this.#pending);
					joined.set(bytes, this.#pending.length);
					bytes = joined;
				}
				const stream = !!options?.stream;
				const [text, left] = native.utf8Decode(bytes, stream);
				this.#pending = left > 0 ? bytes.slice(bytes.length - left) : EMPTY;
				// A byte order mark is dropped where the stream opens only.
				const opening = !this.#started;
				this.#started = stream && (this.#started || text.length > 0);
				return opening && !this.ignoreBOM && text.startsWith("\ufeff") ? text.slice(1) : text;
			}
		};
	}

	// ----- base64 ------------------------------------------------------------------
	if (typeof globalThis.btoa !== "function") {
		globalThis.btoa = (text) => base64Encode(Uint8Array.from(String(text), (c) => c.charCodeAt(0) & 255));
		globalThis.atob = (text) => latin1Decode(base64Decode(text));
	}

	// ----- Buffer ----------------------------------------------------------------------
	class Buffer extends Uint8Array {
		static from(value, encodingOrOffset, length) {
			if (typeof value === "string") return Buffer._fromString(value, encodingOrOffset);
			if (value instanceof ArrayBuffer) return new Buffer(value, encodingOrOffset ?? 0, length ?? value.byteLength - (encodingOrOffset ?? 0));
			if (ArrayBuffer.isView(value)) {
				const out = new Buffer(value.byteLength);
				out.set(new Uint8Array(value.buffer, value.byteOffset, value.byteLength));
				return out;
			}
			if (value && value.type === "Buffer" && Array.isArray(value.data)) return Buffer.from(value.data);
			const out = new Buffer(value.length ?? 0);
			for (let i = 0; i < out.length; i++) out[i] = value[i] & 255;
			return out;
		}
		static _fromString(text, encoding = "utf8") {
			const bytes = Buffer._encode(text, encoding);
			const out = new Buffer(bytes.length);
			out.set(bytes);
			return out;
		}
		static _encode(text, encoding = "utf8") {
			switch (String(encoding).toLowerCase()) {
				case "base64":
					return base64Decode(text);
				case "base64url":
					return base64Decode(text);
				case "hex": {
					const out = new Uint8Array(Math.floor(text.length / 2));
					for (let i = 0; i < out.length; i++) out[i] = parseInt(text.substr(i * 2, 2), 16);
					return out;
				}
				case "latin1":
				case "binary":
				case "ascii":
					return Uint8Array.from(text, (c) => c.charCodeAt(0) & 255);
				default:
					return utf8Encode(text);
			}
		}
		static alloc(size, fill = 0) {
			const out = new Buffer(size);
			if (fill) out.fill(typeof fill === "string" ? fill.charCodeAt(0) : fill);
			return out;
		}
		static allocUnsafe(size) {
			return new Buffer(size);
		}
		static isBuffer(value) {
			return value instanceof Buffer;
		}
		static isEncoding(encoding) {
			return ["utf8", "utf-8", "hex", "base64", "base64url", "latin1", "binary", "ascii"].includes(String(encoding).toLowerCase());
		}
		static byteLength(value, encoding) {
			return typeof value === "string" ? Buffer._encode(value, encoding).length : value.byteLength;
		}
		static concat(list, total) {
			const length = total ?? list.reduce((sum, item) => sum + item.length, 0);
			const out = new Buffer(length);
			let offset = 0;
			for (const item of list) {
				out.set(item.subarray(0, Math.min(item.length, length - offset)), offset);
				offset += item.length;
				if (offset >= length) break;
			}
			return out;
		}
		static compare(a, b) {
			return a.compare(b);
		}
		toString(encoding = "utf8", start = 0, end = this.length) {
			const bytes = this.subarray(start, end);
			switch (String(encoding).toLowerCase()) {
				case "base64":
					return base64Encode(bytes);
				case "base64url":
					return base64Encode(bytes, true);
				case "hex":
					return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
				case "latin1":
				case "binary":
				case "ascii":
					return latin1Decode(bytes);
				default:
					return utf8Decode(bytes);
			}
		}
		toJSON() {
			return { type: "Buffer", data: Array.from(this) };
		}
		equals(other) {
			return this.compare(other) === 0;
		}
		compare(other) {
			const length = Math.min(this.length, other.length);
			for (let i = 0; i < length; i++) if (this[i] !== other[i]) return this[i] < other[i] ? -1 : 1;
			return this.length === other.length ? 0 : this.length < other.length ? -1 : 1;
		}
		slice(start, end) {
			return this.subarray(start, end);
		}
		subarray(start, end) {
			const view = Uint8Array.prototype.subarray.call(this, start, end);
			return new Buffer(view.buffer, view.byteOffset, view.length);
		}
		write(text, offset = 0, encoding = "utf8") {
			const bytes = Buffer._encode(text, encoding);
			this.set(bytes.subarray(0, this.length - offset), offset);
			return Math.min(bytes.length, this.length - offset);
		}
		indexOf(value, from = 0) {
			if (typeof value === "number") return Uint8Array.prototype.indexOf.call(this, value, from);
			const needle = typeof value === "string" ? utf8Encode(value) : value;
			outer: for (let i = from; i <= this.length - needle.length; i++) {
				for (let j = 0; j < needle.length; j++) if (this[i + j] !== needle[j]) continue outer;
				return i;
			}
			return -1;
		}
		includes(value, from) {
			return this.indexOf(value, from) !== -1;
		}
		readUInt8(offset = 0) {
			return this[offset];
		}
		readUInt32LE(offset = 0) {
			return (this[offset] | (this[offset + 1] << 8) | (this[offset + 2] << 16)) + this[offset + 3] * 0x1000000;
		}
		readUInt32BE(offset = 0) {
			return this[offset] * 0x1000000 + ((this[offset + 1] << 16) | (this[offset + 2] << 8) | this[offset + 3]);
		}
		readUInt16LE(offset = 0) {
			return this[offset] | (this[offset + 1] << 8);
		}
		readUInt16BE(offset = 0) {
			return (this[offset] << 8) | this[offset + 1];
		}
	}
	globalThis.Buffer = Buffer;
	yapi.Buffer = Buffer;

	// ----- events and abort signals -----------------------------------------------
	if (typeof globalThis.Event !== "function") {
		globalThis.Event = class Event {
			constructor(type, options = {}) {
				this.type = type;
				this.bubbles = !!options.bubbles;
				this.cancelable = !!options.cancelable;
				this.defaultPrevented = false;
				this.timeStamp = Date.now();
			}
			preventDefault() {
				if (this.cancelable) this.defaultPrevented = true;
			}
			stopPropagation() {}
			stopImmediatePropagation() {}
		};
	}
	if (typeof globalThis.EventTarget !== "function") {
		globalThis.EventTarget = class EventTarget {
			#listeners = new Map();
			addEventListener(type, listener, options) {
				if (!listener) return;
				const list = this.#listeners.get(type) ?? [];
				const once = typeof options === "object" && options?.once;
				if (!list.some((entry) => entry.listener === listener)) list.push({ listener, once });
				this.#listeners.set(type, list);
				const signal = typeof options === "object" ? options?.signal : undefined;
				signal?.addEventListener("abort", () => this.removeEventListener(type, listener), { once: true });
			}
			removeEventListener(type, listener) {
				const list = this.#listeners.get(type);
				if (!list) return;
				this.#listeners.set(
					type,
					list.filter((entry) => entry.listener !== listener),
				);
			}
			dispatchEvent(event) {
				const list = [...(this.#listeners.get(event.type) ?? [])];
				for (const entry of list) {
					if (entry.once) this.removeEventListener(event.type, entry.listener);
					try {
						if (typeof entry.listener === "function") entry.listener.call(this, event);
						else entry.listener.handleEvent(event);
					} catch (error) {
						yapi.log("error", [error]);
					}
				}
				const handler = this[`on${event.type}`];
				if (typeof handler === "function") handler.call(this, event);
				return !event.defaultPrevented;
			}
		};
	}
	if (typeof globalThis.AbortController !== "function") {
		class AbortSignal extends EventTarget {
			constructor() {
				super();
				this.aborted = false;
				this.reason = undefined;
				this.onabort = null;
			}
			throwIfAborted() {
				if (this.aborted) throw this.reason;
			}
			static abort(reason) {
				const controller = new AbortController();
				controller.abort(reason);
				return controller.signal;
			}
			static timeout(ms) {
				const controller = new AbortController();
				setTimeout(() => {
					const error = new Error("The operation was aborted due to timeout");
					error.name = "TimeoutError";
					controller.abort(error);
				}, ms).unref();
				return controller.signal;
			}
			static any(signals) {
				const controller = new AbortController();
				for (const signal of signals) {
					if (signal.aborted) {
						controller.abort(signal.reason);
						break;
					}
					signal.addEventListener("abort", () => controller.abort(signal.reason), { once: true });
				}
				return controller.signal;
			}
		}
		class AbortController {
			constructor() {
				this.signal = new AbortSignal();
			}
			abort(reason) {
				const signal = this.signal;
				if (signal.aborted) return;
				signal.aborted = true;
				if (reason === undefined) {
					reason = new Error("This operation was aborted");
					reason.name = "AbortError";
				}
				signal.reason = reason;
				signal.dispatchEvent(new Event("abort"));
			}
		}
		globalThis.AbortSignal = AbortSignal;
		globalThis.AbortController = AbortController;
	}
	if (typeof globalThis.DOMException !== "function") {
		globalThis.DOMException = class DOMException extends Error {
			constructor(message = "", name = "Error") {
				super(message);
				this.name = name;
			}
		};
	}

	// ----- URL ----------------------------------------------------------------------------
	if (typeof globalThis.URLSearchParams !== "function") {
		globalThis.URLSearchParams = class URLSearchParams {
			#entries = [];
			constructor(init = "") {
				if (typeof init === "string") {
					for (const part of init.replace(/^\?/, "").split("&")) {
						if (!part) continue;
						const [key, value = ""] = part.split("=");
						this.#entries.push([decode(key), decode(value)]);
					}
				} else if (Array.isArray(init)) this.#entries = init.map(([k, v]) => [String(k), String(v)]);
				else if (init) for (const key of Object.keys(init)) this.#entries.push([key, String(init[key])]);
				function decode(text) {
					return decodeURIComponent(text.replace(/\+/g, " "));
				}
			}
			get(key) {
				return this.#entries.find(([k]) => k === key)?.[1] ?? null;
			}
			getAll(key) {
				return this.#entries.filter(([k]) => k === key).map(([, v]) => v);
			}
			has(key) {
				return this.#entries.some(([k]) => k === key);
			}
			set(key, value) {
				this.delete(key);
				this.#entries.push([key, String(value)]);
			}
			append(key, value) {
				this.#entries.push([key, String(value)]);
			}
			delete(key) {
				this.#entries = this.#entries.filter(([k]) => k !== key);
			}
			entries() {
				return this.#entries[Symbol.iterator]();
			}
			keys() {
				return this.#entries.map(([k]) => k)[Symbol.iterator]();
			}
			values() {
				return this.#entries.map(([, v]) => v)[Symbol.iterator]();
			}
			forEach(callback) {
				for (const [k, v] of this.#entries) callback(v, k, this);
			}
			[Symbol.iterator]() {
				return this.entries();
			}
			toString() {
				const encode = (text) => encodeURIComponent(text).replace(/%20/g, "+");
				return this.#entries.map(([k, v]) => `${encode(k)}=${encode(v)}`).join("&");
			}
		};
	}
	if (typeof globalThis.URL !== "function") {
		const PATTERN = /^([a-zA-Z][a-zA-Z0-9+.-]*:)(?:\/\/(?:([^:@/]*)(?::([^@/]*))?@)?(\[[^\]]+\]|[^:/?#]*)(?::(\d+))?)?([^?#]*)(\?[^#]*)?(#.*)?$/;
		globalThis.URL = class URL {
			constructor(input, base) {
				let text = String(input);
				if (!/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(text)) {
					if (base === undefined) throw new TypeError(`Invalid URL: ${text}`);
					const parent = new URL(base);
					if (text.startsWith("//")) text = parent.protocol + text;
					else if (text.startsWith("/")) text = `${parent.origin}${text}`;
					else if (text.startsWith("?")) text = `${parent.origin}${parent.pathname}${text}`;
					else if (text.startsWith("#")) text = `${parent.origin}${parent.pathname}${parent.search}${text}`;
					else {
						const dir = parent.pathname.replace(/[^/]*$/, "");
						text = `${parent.protocol === "file:" ? "file://" : parent.origin}${dir}${text}`;
					}
				}
				const match = PATTERN.exec(text);
				if (!match) throw new TypeError(`Invalid URL: ${text}`);
				this.protocol = match[1].toLowerCase();
				this.username = match[2] ?? "";
				this.password = match[3] ?? "";
				this.hostname = (match[4] ?? "").toLowerCase();
				this.port = match[5] ?? "";
				this.pathname = normalizePath(match[6] || (this.hostname ? "/" : ""));
				this.hash = match[8] ?? "";
				this.searchParams = new URLSearchParams(match[7] ?? "");
				this._search = match[7] ?? "";
				function normalizePath(path) {
					const segments = [];
					for (const segment of path.split("/")) {
						if (segment === "..") segments.pop();
						else if (segment !== ".") segments.push(segment);
					}
					const out = segments.join("/");
					return path.startsWith("/") && !out.startsWith("/") ? `/${out}` : out;
				}
			}
			get search() {
				const text = this.searchParams.toString();
				return text ? `?${text}` : "";
			}
			set search(value) {
				this.searchParams = new URLSearchParams(value);
			}
			get host() {
				return this.port ? `${this.hostname}:${this.port}` : this.hostname;
			}
			get origin() {
				return this.protocol === "file:" ? "file://" : `${this.protocol}//${this.host}`;
			}
			get href() {
				const auth = this.username ? `${this.username}${this.password ? `:${this.password}` : ""}@` : "";
				const authority = this.protocol === "file:" || this.hostname ? `//${auth}${this.host}` : "";
				return `${this.protocol}${authority}${this.pathname}${this.search}${this.hash}`;
			}
			toString() {
				return this.href;
			}
			toJSON() {
				return this.href;
			}
			static canParse(input, base) {
				try {
					new URL(input, base);
					return true;
				} catch {
					return false;
				}
			}
		};
	}

	// ----- Intl ------------------------------------------------------------------------
	// QuickJS has no Intl. Segmentation follows Unicode's rules (natively);
	// formatting is en-US.
	if (typeof globalThis.Intl !== "object") {
		class Segments {
			#items;
			constructor(input, granularity) {
				let index = 0;
				this.#items = native.segment(input, granularity).map((segment) => {
					const item = { segment, index, input };
					if (granularity === "word") item.isWordLike = /[\p{L}\p{N}]/u.test(segment);
					index += segment.length;
					return item;
				});
			}
			containing(index = 0) {
				return this.#items.find((item) => index >= item.index && index < item.index + item.segment.length);
			}
			[Symbol.iterator]() {
				return this.#items[Symbol.iterator]();
			}
		}
		class Segmenter {
			#granularity;
			constructor(_locales, options = {}) {
				this.#granularity = options.granularity ?? "grapheme";
			}
			segment(input) {
				return new Segments(String(input), this.#granularity);
			}
			resolvedOptions() {
				return { locale: "en-US", granularity: this.#granularity };
			}
			static supportedLocalesOf(locales) {
				return [].concat(locales ?? []);
			}
		}
		const group = (digits) => digits.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
		class NumberFormat {
			#options;
			constructor(_locales, options = {}) {
				this.#options = options;
			}
			format(value) {
				const options = this.#options;
				let number = Number(value);
				if (!Number.isFinite(number)) return String(number);
				if (options.style === "percent") number *= 100;
				const max = options.maximumFractionDigits ?? Math.max(options.minimumFractionDigits ?? 0, options.style === "percent" ? 0 : 3);
				let text = number.toFixed(max);
				if (text.includes(".")) text = text.replace(/0+$/, "").replace(/\.$/, "");
				const min = options.minimumFractionDigits ?? 0;
				if (min > 0) {
					const [whole, fraction = ""] = text.split(".");
					text = `${whole}.${fraction.padEnd(min, "0")}`;
				}
				const [whole, fraction] = text.split(".");
				const sign = whole.startsWith("-") ? "-" : "";
				const digits = sign ? whole.slice(1) : whole;
				text = sign + (options.useGrouping === false ? digits : group(digits)) + (fraction ? `.${fraction}` : "");
				if (options.style === "percent") return `${text}%`;
				if (options.style === "currency") return `${options.currency === "USD" || !options.currency ? "$" : `${options.currency} `}${text}`;
				return text;
			}
			formatToParts(value) {
				return [{ type: "integer", value: this.format(value) }];
			}
			resolvedOptions() {
				return { locale: "en-US", numberingSystem: "latn", ...this.#options };
			}
			static supportedLocalesOf(locales) {
				return [].concat(locales ?? []);
			}
		}
		class DateTimeFormat {
			#options;
			constructor(_locales, options = {}) {
				this.#options = options;
			}
			format(date) {
				return new Date(date ?? Date.now()).toLocaleString();
			}
			formatToParts(date) {
				return [{ type: "literal", value: this.format(date) }];
			}
			resolvedOptions() {
				return { locale: "en-US", calendar: "gregory", numberingSystem: "latn", timeZone: globalThis.process?.env?.TZ || "UTC", ...this.#options };
			}
			static supportedLocalesOf(locales) {
				return [].concat(locales ?? []);
			}
		}
		class Collator {
			#options;
			constructor(_locales, options = {}) {
				this.#options = options;
				this.compare = this.compare.bind(this);
			}
			compare(a, b) {
				let left = String(a);
				let right = String(b);
				if (this.#options.sensitivity === "base" || this.#options.sensitivity === "accent") {
					left = left.toLowerCase();
					right = right.toLowerCase();
				}
				if (this.#options.numeric) {
					const split = (text) => text.split(/(\d+)/).map((part, index) => (index % 2 ? Number(part) : part));
					const x = split(left);
					const y = split(right);
					for (let i = 0; i < Math.min(x.length, y.length); i++) if (x[i] !== y[i]) return x[i] < y[i] ? -1 : 1;
					return x.length - y.length;
				}
				return left < right ? -1 : left > right ? 1 : 0;
			}
			resolvedOptions() {
				return { locale: "en-US", ...this.#options };
			}
		}
		class PluralRules {
			select(value) {
				return Number(value) === 1 ? "one" : "other";
			}
		}
		class RelativeTimeFormat {
			format(value, unit) {
				const name = String(unit).replace(/s$/, "");
				const count = Math.abs(value);
				const label = `${count} ${name}${count === 1 ? "" : "s"}`;
				return value < 0 ? `${label} ago` : `in ${label}`;
			}
		}
		globalThis.Intl = {
			Segmenter,
			NumberFormat,
			DateTimeFormat,
			Collator,
			PluralRules,
			RelativeTimeFormat,
			getCanonicalLocales: (locales) => [].concat(locales ?? []),
			supportedValuesOf: () => [],
		};
	}

	// ----- crypto and performance ---------------------------------------------------
	const randomBytes = (count) => base64Decode(yapi.request("random", { bytes: count }));
	yapi.randomBytes = randomBytes;
	if (typeof globalThis.crypto !== "object") {
		globalThis.crypto = {
			getRandomValues(array) {
				const bytes = randomBytes(array.byteLength);
				new Uint8Array(array.buffer, array.byteOffset, array.byteLength).set(bytes);
				return array;
			},
			randomUUID() {
				const bytes = randomBytes(16);
				bytes[6] = (bytes[6] & 0x0f) | 0x40;
				bytes[8] = (bytes[8] & 0x3f) | 0x80;
				const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
				return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
			},
		};
	}
	if (typeof globalThis.performance !== "object") {
		const origin = Date.now();
		globalThis.performance = { now: () => Date.now() - origin, timeOrigin: origin, mark() {}, measure() {} };
	}

	// ----- process --------------------------------------------------------------------------
	const environment = yapi.request("env") ?? {};
	// 10-node.js makes it an EventEmitter, as in Node.
	globalThis.process = {
		env: environment,
		argv: ["yapi", "extension"],
		execArgv: [],
		argv0: "yapi",
		execPath: "/usr/bin/yapi",
		platform: yapi.request("platform") ?? "linux",
		arch: "wasm32",
		pid: 1,
		ppid: 0,
		version: "v22.0.0",
		versions: { node: "22.0.0", yapi: "0.1.0" },
		release: { name: "node" },
		// Node's diagnostic report, which probes such as detect-libc read: no
		// C library to name, and no native libraries loaded.
		report: {
			excludeNetwork: false,
			getReport: () => ({ header: { reportVersion: 3, nodejsVersion: "v22.0.0", arch: "wasm32" }, javascriptStack: {}, sharedObjects: [] }),
			writeReport: () => "",
		},
		features: {},
		exitCode: undefined,
		title: "yapi",
		cwd: () => yapi.cwd ?? yapi.request("cwd"),
		chdir() {
			throw new Error("process.chdir() is not supported in yapi extensions");
		},
		exit(code) {
			const error = new Error(`process.exit(${code ?? 0}) called`);
			error.code = "ERR_PROCESS_EXIT";
			throw error;
		},
		nextTick: (callback, ...args) => queueMicrotask(() => callback(...args)),
		hrtime: Object.assign(
			(previous) => {
				const now = performance.now();
				const seconds = Math.floor(now / 1000);
				const nanos = Math.floor((now % 1000) * 1e6);
				if (!previous) return [seconds, nanos];
				let s = seconds - previous[0];
				let n = nanos - previous[1];
				if (n < 0) {
					s--;
					n += 1e9;
				}
				return [s, n];
			},
			{ bigint: () => BigInt(Math.floor(performance.now() * 1e6)) },
		),
		uptime: () => performance.now() / 1000,
		memoryUsage: () => ({ rss: 0, heapTotal: 0, heapUsed: 0, external: 0, arrayBuffers: 0 }),
		cpuUsage: () => ({ user: 0, system: 0 }),
		umask: () => 0o22,
		getuid: () => 0,
		getgid: () => 0,
		emitWarning: (warning) => yapi.log("warn", [warning]),
		getBuiltinModule: (name) => globalThis.__yapi_builtins[String(name).replace(/^node:/, "")],
		stdout: {
			isTTY: false,
			columns: 80,
			rows: 24,
			write(text) {
				yapi.log("info", [String(text).replace(/\n$/, "")]);
				return true;
			},
			on() {
				return this;
			},
			once() {
				return this;
			},
			off() {
				return this;
			},
			removeListener() {
				return this;
			},
		},
		stderr: {
			isTTY: false,
			columns: 80,
			write(text) {
				yapi.log("warn", [String(text).replace(/\n$/, "")]);
				return true;
			},
			on() {
				return this;
			},
		},
		stdin: {
			isTTY: false,
			on() {
				return this;
			},
			once() {
				return this;
			},
			off() {
				return this;
			},
			removeListener() {
				return this;
			},
			setRawMode() {
				return this;
			},
			resume() {
				return this;
			},
			pause() {
				return this;
			},
		},
	};
	globalThis.global = globalThis;

	// ----- web streams ------------------------------------------------------------------------
	// Enough of the WHATWG streams for extensions and their libraries: sources,
	// sinks and transformers run in order; queues are unbounded.
	class ReadableStream {
		#source;
		#controller;
		#queue = [];
		#waiters = [];
		#started = false;
		#pulling = false;
		#closed = false;
		#failure;
		#failed = false;
		#locked = false;
		#highWaterMark;
		#ended;
		#end;
		constructor(source = {}, strategy = {}) {
			this.#ended = new Promise((resolve, reject) => (this.#end = { resolve, reject }));
			this.#ended.catch(() => {});
			this.#source = source ?? {};
			this.#highWaterMark = strategy?.highWaterMark ?? 1;
			const stream = this;
			this.#controller = {
				enqueue: (chunk) => stream.#enqueue(chunk),
				close: () => stream.#close(),
				error: (error) => stream.#fail(error),
				get desiredSize() {
					return stream.#highWaterMark - stream.#queue.length;
				},
			};
			let started;
			try {
				started = this.#source.start?.(this.#controller);
			} catch (error) {
				this.#fail(error);
			}
			Promise.resolve(started).then(
				() => {
					this.#started = true;
					this.#pull();
				},
				(error) => this.#fail(error),
			);
		}
		static from(iterable) {
			const iterator = iterable[Symbol.asyncIterator] ? iterable[Symbol.asyncIterator]() : iterable[Symbol.iterator]();
			return new ReadableStream({
				async pull(controller) {
					const { value, done } = await iterator.next();
					if (done) controller.close();
					else controller.enqueue(value);
				},
				cancel: (reason) => iterator.return?.(reason),
			});
		}
		get locked() {
			return this.#locked;
		}
		#enqueue(chunk) {
			if (this.#closed || this.#failed) throw new TypeError("Cannot enqueue to a closed stream");
			const waiter = this.#waiters.shift();
			if (waiter) waiter.resolve({ value: chunk, done: false });
			else this.#queue.push(chunk);
		}
		#close() {
			this.#closed = true;
			this.#end.resolve();
			if (this.#queue.length === 0) for (const waiter of this.#waiters.splice(0)) waiter.resolve({ value: undefined, done: true });
		}
		#fail(error) {
			if (this.#failed) return;
			this.#failed = true;
			this.#failure = error;
			this.#end.reject(error);
			this.#queue = [];
			for (const waiter of this.#waiters.splice(0)) waiter.reject(error);
		}
		#pull() {
			if (!this.#started || this.#pulling || this.#closed || this.#failed || typeof this.#source.pull !== "function") return;
			if (this.#waiters.length === 0 && this.#queue.length >= this.#highWaterMark) return;
			this.#pulling = true;
			Promise.resolve()
				.then(() => this.#source.pull(this.#controller))
				.then(
					() => {
						this.#pulling = false;
						if (this.#waiters.length > 0) this.#pull();
					},
					(error) => this.#fail(error),
				);
		}
		#read() {
			if (this.#queue.length > 0) {
				const value = this.#queue.shift();
				if (this.#closed && this.#queue.length === 0) for (const waiter of this.#waiters.splice(0)) waiter.resolve({ value: undefined, done: true });
				this.#pull();
				return Promise.resolve({ value, done: false });
			}
			if (this.#failed) return Promise.reject(this.#failure);
			if (this.#closed) return Promise.resolve({ value: undefined, done: true });
			return new Promise((resolve, reject) => {
				this.#waiters.push({ resolve, reject });
				this.#pull();
			});
		}
		cancel(reason) {
			this.#closed = true;
			this.#end.resolve();
			this.#queue = [];
			for (const waiter of this.#waiters.splice(0)) waiter.resolve({ value: undefined, done: true });
			return Promise.resolve(this.#source.cancel?.(reason));
		}
		getReader() {
			if (this.#locked) throw new TypeError("ReadableStream is locked");
			this.#locked = true;
			const stream = this;
			let released = false;
			const closed = stream.#ended;
			return {
				read: () => (released ? Promise.reject(new TypeError("Reader released")) : stream.#read()),
				cancel: (reason) => stream.cancel(reason),
				releaseLock() {
					released = true;
					stream.#locked = false;
				},
				closed,
			};
		}
		async *values({ preventCancel = false } = {}) {
			const reader = this.getReader();
			try {
				while (true) {
					const { value, done } = await reader.read();
					if (done) return;
					yield value;
				}
			} finally {
				if (!preventCancel) await this.cancel();
				reader.releaseLock();
			}
		}
		[Symbol.asyncIterator](options) {
			return this.values(options);
		}
		pipeThrough(transform, options) {
			this.pipeTo(transform.writable, options).catch(() => {});
			return transform.readable;
		}
		async pipeTo(destination, { preventClose = false, preventAbort = false, signal } = {}) {
			const reader = this.getReader();
			const writer = destination.getWriter();
			try {
				while (true) {
					if (signal?.aborted) throw signal.reason;
					const { value, done } = await reader.read();
					if (done) break;
					await writer.write(value);
				}
				if (!preventClose) await writer.close();
			} catch (error) {
				if (!preventAbort) await writer.abort(error);
				throw error;
			} finally {
				reader.releaseLock();
				writer.releaseLock();
			}
		}
		tee() {
			const reader = this.getReader();
			const controllers = [];
			const branch = () =>
				new ReadableStream({
					start(controller) {
						controllers.push(controller);
					},
				});
			const branches = [branch(), branch()];
			(async () => {
				try {
					while (true) {
						const { value, done } = await reader.read();
						if (done) break;
						for (const controller of controllers) controller.enqueue(value);
					}
					for (const controller of controllers) controller.close();
				} catch (error) {
					for (const controller of controllers) controller.error(error);
				}
			})();
			return branches;
		}
	}

	class WritableStream {
		#sink;
		#controller;
		#chain;
		#locked = false;
		constructor(sink = {}) {
			this.#sink = sink ?? {};
			const abort = new AbortController();
			this.#controller = { error: (error) => abort.abort(error), signal: abort.signal };
			this.#chain = Promise.resolve().then(() => this.#sink.start?.(this.#controller));
		}
		get locked() {
			return this.#locked;
		}
		#then(step) {
			const next = this.#chain.then(step);
			this.#chain = next.catch(() => {});
			return next;
		}
		abort(reason) {
			return Promise.resolve(this.#sink.abort?.(reason));
		}
		close() {
			return this.#then(() => this.#sink.close?.());
		}
		getWriter() {
			if (this.#locked) throw new TypeError("WritableStream is locked");
			this.#locked = true;
			const stream = this;
			return {
				write: (chunk) => stream.#then(() => stream.#sink.write?.(chunk, stream.#controller)),
				close: () => stream.close(),
				abort: (reason) => stream.abort(reason),
				releaseLock() {
					stream.#locked = false;
				},
				get ready() {
					return Promise.resolve();
				},
				get closed() {
					return stream.#chain;
				},
				desiredSize: 1,
			};
		}
	}

	class TransformStream {
		constructor(transformer = {}, writableStrategy, readableStrategy) {
			transformer ??= {};
			let output;
			this.readable = new ReadableStream({ start: (controller) => void (output = controller) }, readableStrategy);
			const controller = {
				enqueue: (chunk) => output.enqueue(chunk),
				error: (error) => output.error(error),
				terminate: () => output.close(),
				get desiredSize() {
					return output.desiredSize;
				},
			};
			const started = Promise.resolve().then(() => transformer.start?.(controller));
			this.writable = new WritableStream(
				{
					write: async (chunk) => {
						await started;
						if (typeof transformer.transform === "function") await transformer.transform(chunk, controller);
						else controller.enqueue(chunk);
					},
					close: async () => {
						await started;
						await transformer.flush?.(controller);
						output.close();
					},
					abort: (reason) => output.error(reason),
				},
				writableStrategy,
			);
		}
	}

	class TextEncoderStream extends TransformStream {
		constructor() {
			const encoder = new TextEncoder();
			super({ transform: (chunk, controller) => controller.enqueue(encoder.encode(String(chunk))) });
			this.encoding = "utf-8";
		}
	}
	class TextDecoderStream extends TransformStream {
		constructor(label = "utf-8", options = {}) {
			const decoder = new TextDecoder(label, options);
			super({
				transform: (chunk, controller) => {
					const text = decoder.decode(chunk, { stream: true });
					if (text) controller.enqueue(text);
				},
				flush: (controller) => {
					const text = decoder.decode();
					if (text) controller.enqueue(text);
				},
			});
			this.encoding = decoder.encoding ?? label;
		}
	}
	class CountQueuingStrategy {
		constructor({ highWaterMark }) {
			this.highWaterMark = highWaterMark;
		}
		size() {
			return 1;
		}
	}
	class ByteLengthQueuingStrategy {
		constructor({ highWaterMark }) {
			this.highWaterMark = highWaterMark;
		}
		size(chunk) {
			return chunk.byteLength;
		}
	}
	const webStreams = { ReadableStream, WritableStream, TransformStream, TextEncoderStream, TextDecoderStream, CountQueuingStrategy, ByteLengthQueuingStrategy };
	Object.assign(globalThis, webStreams);
	yapi.webStreams = webStreams;

	// ----- MessageChannel: ports within the instance ----------------------------------------------
	class MessageEvent extends Event {
		constructor(type, init = {}) {
			super(type, init);
			this.data = init.data;
		}
	}
	class MessagePort extends EventTarget {
		constructor() {
			super();
			this.onmessage = null;
			this._other = null;
			this._closed = false;
		}
		postMessage(data) {
			const other = this._other;
			if (!other || this._closed) return;
			const copy = structuredClone(data);
			queueMicrotask(() => {
				if (other._closed) return;
				const event = new MessageEvent("message", { data: copy });
				other.onmessage?.(event);
				other.dispatchEvent(event);
			});
		}
		start() {}
		close() {
			this._closed = true;
		}
		ref() {
			return this;
		}
		unref() {
			return this;
		}
	}
	class MessageChannel {
		constructor() {
			this.port1 = new MessagePort();
			this.port2 = new MessagePort();
			this.port1._other = this.port2;
			this.port2._other = this.port1;
		}
	}
	class BroadcastChannel extends EventTarget {
		constructor(name) {
			super();
			this.name = String(name);
			this.onmessage = null;
		}
		postMessage() {}
		close() {}
		ref() {
			return this;
		}
		unref() {
			return this;
		}
	}
	Object.assign(globalThis, { MessageEvent, MessagePort, MessageChannel, BroadcastChannel });

	// ----- Blob, File and FormData --------------------------------------------------------------
	const blobBytes = (part) => {
		if (part instanceof Blob) return part._bytes;
		if (part instanceof Uint8Array) return part;
		if (part instanceof ArrayBuffer) return new Uint8Array(part);
		if (ArrayBuffer.isView(part)) return new Uint8Array(part.buffer, part.byteOffset, part.byteLength);
		return utf8Encode(String(part));
	};
	class Blob {
		constructor(parts = [], options = {}) {
			const chunks = [...parts].map(blobBytes);
			const bytes = new Uint8Array(chunks.reduce((total, chunk) => total + chunk.length, 0));
			let offset = 0;
			for (const chunk of chunks) {
				bytes.set(chunk, offset);
				offset += chunk.length;
			}
			this._bytes = bytes;
			this.type = String(options?.type ?? "").toLowerCase();
		}
		get size() {
			return this._bytes.length;
		}
		async text() {
			return utf8Decode(this._bytes);
		}
		async arrayBuffer() {
			return this._bytes.slice().buffer;
		}
		async bytes() {
			return this._bytes.slice();
		}
		slice(start = 0, end = this._bytes.length, type = "") {
			return new Blob([this._bytes.slice(start, end)], { type });
		}
		stream() {
			const bytes = this._bytes;
			return new ReadableStream({
				start(controller) {
					if (bytes.length > 0) controller.enqueue(bytes.slice());
					controller.close();
				},
			});
		}
		get [Symbol.toStringTag]() {
			return "Blob";
		}
	}
	class File extends Blob {
		constructor(parts, name, options = {}) {
			super(parts, options);
			this.name = String(name);
			this.lastModified = options?.lastModified ?? Date.now();
		}
		get [Symbol.toStringTag]() {
			return "File";
		}
	}
	class FormData {
		#entries = [];
		append(name, value, filename) {
			this.#entries.push([String(name), value instanceof Blob && filename !== undefined ? new File([value], filename) : value]);
		}
		set(name, value, filename) {
			this.delete(name);
			this.append(name, value, filename);
		}
		get(name) {
			return this.#entries.find(([key]) => key === name)?.[1] ?? null;
		}
		getAll(name) {
			return this.#entries.filter(([key]) => key === name).map(([, value]) => value);
		}
		has(name) {
			return this.#entries.some(([key]) => key === name);
		}
		delete(name) {
			this.#entries = this.#entries.filter(([key]) => key !== name);
		}
		*entries() {
			yield* this.#entries;
		}
		*keys() {
			for (const [key] of this.#entries) yield key;
		}
		*values() {
			for (const [, value] of this.#entries) yield value;
		}
		forEach(callback, thisArg) {
			for (const [key, value] of this.#entries) callback.call(thisArg, value, key, this);
		}
		[Symbol.iterator]() {
			return this.entries();
		}
	}
	Object.assign(globalThis, { Blob, File, FormData });

	// ----- fetch ------------------------------------------------------------------------------
	class Headers {
		#map = new Map();
		constructor(init) {
			if (!init) return;
			const entries = init instanceof Headers ? [...init] : Array.isArray(init) ? init : Object.entries(init);
			for (const [key, value] of entries) this.append(key, value);
		}
		append(key, value) {
			const name = String(key).toLowerCase();
			const existing = this.#map.get(name);
			this.#map.set(name, existing === undefined ? String(value) : `${existing}, ${value}`);
		}
		set(key, value) {
			this.#map.set(String(key).toLowerCase(), String(value));
		}
		get(key) {
			return this.#map.get(String(key).toLowerCase()) ?? null;
		}
		has(key) {
			return this.#map.has(String(key).toLowerCase());
		}
		delete(key) {
			this.#map.delete(String(key).toLowerCase());
		}
		forEach(callback) {
			for (const [key, value] of this.#map) callback(value, key, this);
		}
		entries() {
			return this.#map.entries();
		}
		keys() {
			return this.#map.keys();
		}
		values() {
			return this.#map.values();
		}
		[Symbol.iterator]() {
			return this.#map.entries();
		}
	}
	class Response {
		constructor(body = null, init = {}) {
			this._bytes = body == null ? new Uint8Array() : typeof body === "string" ? utf8Encode(body) : body instanceof Uint8Array ? body : utf8Encode(String(body));
			this.status = init.status ?? 200;
			this.statusText = init.statusText ?? "";
			this.headers = new Headers(init.headers);
			this.ok = this.status >= 200 && this.status < 300;
			this.url = init.url ?? "";
			this.redirected = false;
			this.bodyUsed = false;
			this.type = "basic";
		}
		async text() {
			this.bodyUsed = true;
			return utf8Decode(this._bytes);
		}
		async json() {
			return JSON.parse(await this.text());
		}
		async arrayBuffer() {
			this.bodyUsed = true;
			return this._bytes.slice().buffer;
		}
		async bytes() {
			this.bodyUsed = true;
			return this._bytes.slice();
		}
		get body() {
			const bytes = this._bytes;
			return new ReadableStream({
				start(controller) {
					if (bytes.length > 0) controller.enqueue(bytes);
					controller.close();
				},
			});
		}
		clone() {
			return new Response(this._bytes.slice(), { status: this.status, statusText: this.statusText, headers: this.headers, url: this.url });
		}
	}
	class Request {
		constructor(input, init = {}) {
			this.url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
			this.method = (init.method ?? input?.method ?? "GET").toUpperCase();
			this.headers = new Headers(init.headers ?? input?.headers);
			this.body = init.body ?? input?.body;
			this.signal = init.signal ?? input?.signal;
		}
	}
	globalThis.Headers = Headers;
	globalThis.Response = Response;
	globalThis.Request = Request;
	globalThis.fetch = async (input, init = {}) => {
		const request = new Request(input, init);
		request.signal?.throwIfAborted?.();
		let body = request.body;
		let bodyBase64;
		if (body instanceof URLSearchParams) {
			if (!request.headers.has("content-type")) request.headers.set("content-type", "application/x-www-form-urlencoded;charset=UTF-8");
			body = body.toString();
		}
		if (body instanceof Uint8Array || body instanceof ArrayBuffer) {
			bodyBase64 = base64Encode(body instanceof ArrayBuffer ? new Uint8Array(body) : body);
			body = undefined;
		}
		const result = await yapi.op("fetch", {
			url: request.url,
			method: request.method,
			headers: Object.fromEntries(request.headers),
			body: typeof body === "string" ? body : undefined,
			bodyBase64,
		});
		return new Response(base64Decode(result.bodyBase64), {
			status: result.status,
			statusText: result.statusText,
			headers: result.headers,
			url: request.url,
		});
	};
})();
