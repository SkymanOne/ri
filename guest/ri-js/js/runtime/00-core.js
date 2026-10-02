// Core globals for extensions running in ri-js: host plumbing, timers,
// console, process, encodings, Buffer, abort signals, URL and fetch.
//
// `__ri_native` comes from the Rust side: `request(kind, json)` answers at
// once, `start(kind, json)` begins an operation and returns its id, `done` and
// `fail` finish calls, and `fs(op, json)` reaches the filesystem.
(() => {
	"use strict";
	const native = globalThis.__ri_native;
	const ri = {};
	globalThis.__ri = ri;

	// ----- host plumbing -------------------------------------------------
	const pendingOps = new Map();
	ri.request = (kind, payload) => {
		const text = native.request(kind, JSON.stringify(payload ?? null));
		return text === "" ? undefined : JSON.parse(text);
	};
	/** Starts a host operation; resolves with its JSON result. */
	ri.op = (kind, payload) =>
		new Promise((resolve, reject) => {
			const id = native.start(kind, JSON.stringify(payload ?? null));
			pendingOps.set(id, { resolve, reject });
		});
	ri.resolve = (op, ok, text) => {
		const pending = pendingOps.get(op);
		if (!pending) return;
		pendingOps.delete(op);
		if (ok) pending.resolve(text === "" ? undefined : JSON.parse(text));
		else pending.reject(new Error(text));
	};
	ri.log = (level, args) => {
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
	ri.formatValue = formatValue;
	ri.format = (format, ...args) => {
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
		log: (...args) => ri.log("info", [ri.format(...args)]),
		info: (...args) => ri.log("info", [ri.format(...args)]),
		debug: (...args) => ri.log("debug", [ri.format(...args)]),
		warn: (...args) => ri.log("warn", [ri.format(...args)]),
		error: (...args) => ri.log("error", [ri.format(...args)]),
		trace: (...args) => ri.log("debug", [ri.format(...args)]),
		dir: (value) => ri.log("info", [formatValue(value)]),
		table: (value) => ri.log("info", [formatValue(value)]),
		time() {},
		timeEnd() {},
		timeLog() {},
		assert(condition, ...args) {
			if (!condition) ri.log("error", ["Assertion failed", ...args]);
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
			ri.op("timer", { ms: delay }).then(() => {
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
	function utf8Encode(text) {
		const out = [];
		for (let i = 0; i < text.length; i++) {
			let code = text.charCodeAt(i);
			if (code >= 0xd800 && code <= 0xdbff && i + 1 < text.length) {
				const next = text.charCodeAt(i + 1);
				if (next >= 0xdc00 && next <= 0xdfff) {
					code = 0x10000 + ((code - 0xd800) << 10) + (next - 0xdc00);
					i++;
				} else code = 0xfffd;
			} else if (code >= 0xd800 && code <= 0xdfff) code = 0xfffd;
			if (code < 0x80) out.push(code);
			else if (code < 0x800) out.push(0xc0 | (code >> 6), 0x80 | (code & 63));
			else if (code < 0x10000) out.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 63), 0x80 | (code & 63));
			else out.push(0xf0 | (code >> 18), 0x80 | ((code >> 12) & 63), 0x80 | ((code >> 6) & 63), 0x80 | (code & 63));
		}
		return new Uint8Array(out);
	}
	function utf8Decode(bytes) {
		let out = "";
		let i = 0;
		const chunk = [];
		const flush = () => {
			out += String.fromCharCode.apply(null, chunk);
			chunk.length = 0;
		};
		while (i < bytes.length) {
			const byte = bytes[i++];
			let code;
			if (byte < 0x80) code = byte;
			else if (byte >= 0xc2 && byte < 0xe0 && i < bytes.length && (bytes[i] & 0xc0) === 0x80) code = ((byte & 31) << 6) | (bytes[i++] & 63);
			else if (byte >= 0xe0 && byte < 0xf0 && i + 1 < bytes.length && (bytes[i] & 0xc0) === 0x80 && (bytes[i + 1] & 0xc0) === 0x80) {
				code = ((byte & 15) << 12) | ((bytes[i] & 63) << 6) | (bytes[i + 1] & 63);
				i += 2;
				if (code < 0x800 || (code >= 0xd800 && code <= 0xdfff)) code = 0xfffd;
			} else if (byte >= 0xf0 && byte < 0xf5 && i + 2 < bytes.length && (bytes[i] & 0xc0) === 0x80 && (bytes[i + 1] & 0xc0) === 0x80 && (bytes[i + 2] & 0xc0) === 0x80) {
				code = ((byte & 7) << 18) | ((bytes[i] & 63) << 12) | ((bytes[i + 1] & 63) << 6) | (bytes[i + 2] & 63);
				i += 3;
				if (code < 0x10000 || code > 0x10ffff) code = 0xfffd;
			} else code = 0xfffd;
			if (code > 0xffff) {
				code -= 0x10000;
				chunk.push(0xd800 + (code >> 10), 0xdc00 + (code & 1023));
			} else chunk.push(code);
			if (chunk.length > 8192) flush();
		}
		flush();
		return out;
	}
	ri.utf8Encode = utf8Encode;
	ri.utf8Decode = utf8Decode;
	if (typeof globalThis.TextEncoder !== "function") {
		globalThis.TextEncoder = class TextEncoder {
			get encoding() {
				return "utf-8";
			}
			encode(text = "") {
				return utf8Encode(String(text));
			}
		};
	}
	if (typeof globalThis.TextDecoder !== "function") {
		globalThis.TextDecoder = class TextDecoder {
			constructor(encoding = "utf-8") {
				this.encoding = String(encoding).toLowerCase();
			}
			decode(input) {
				if (input === undefined) return "";
				const bytes = input instanceof Uint8Array ? input : ArrayBuffer.isView(input) ? new Uint8Array(input.buffer, input.byteOffset, input.byteLength) : new Uint8Array(input);
				if (this.encoding === "latin1" || this.encoding === "ascii") return String.fromCharCode(...bytes);
				return utf8Decode(bytes);
			}
		};
	}

	// ----- base64 ------------------------------------------------------------------
	const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	const B64_INDEX = new Map([...B64].map((c, i) => [c, i]));
	B64_INDEX.set("-", 62);
	B64_INDEX.set("_", 63);
	function base64Encode(bytes, url = false) {
		let out = "";
		for (let i = 0; i < bytes.length; i += 3) {
			const a = bytes[i];
			const b = i + 1 < bytes.length ? bytes[i + 1] : 0;
			const c = i + 2 < bytes.length ? bytes[i + 2] : 0;
			const triple = (a << 16) | (b << 8) | c;
			out += B64[(triple >> 18) & 63] + B64[(triple >> 12) & 63];
			out += i + 1 < bytes.length ? B64[(triple >> 6) & 63] : url ? "" : "=";
			out += i + 2 < bytes.length ? B64[triple & 63] : url ? "" : "=";
		}
		return url ? out.replace(/\+/g, "-").replace(/\//g, "_") : out;
	}
	function base64Decode(text) {
		const clean = String(text).replace(/[^A-Za-z0-9+/_-]/g, "");
		const out = [];
		let buffer = 0;
		let bits = 0;
		for (const c of clean) {
			buffer = (buffer << 6) | B64_INDEX.get(c);
			bits += 6;
			if (bits >= 8) {
				bits -= 8;
				out.push((buffer >> bits) & 255);
			}
		}
		return new Uint8Array(out);
	}
	ri.base64Encode = base64Encode;
	ri.base64Decode = base64Decode;
	if (typeof globalThis.btoa !== "function") {
		globalThis.btoa = (text) => base64Encode(Uint8Array.from(String(text), (c) => c.charCodeAt(0) & 255));
		globalThis.atob = (text) => String.fromCharCode(...base64Decode(text));
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
					return String.fromCharCode(...bytes);
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
	ri.Buffer = Buffer;

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
						ri.log("error", [error]);
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
	const randomBytes = (count) => base64Decode(ri.request("random", { bytes: count }));
	ri.randomBytes = randomBytes;
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
	const environment = ri.request("env") ?? {};
	const listeners = new Map();
	globalThis.process = {
		env: environment,
		argv: ["ri", "extension"],
		execArgv: [],
		argv0: "ri",
		execPath: "/usr/bin/ri",
		platform: ri.request("platform") ?? "linux",
		arch: "wasm32",
		pid: 1,
		ppid: 0,
		version: "v22.0.0",
		versions: { node: "22.0.0", ri: "0.1.0" },
		release: { name: "node" },
		features: {},
		exitCode: undefined,
		title: "ri",
		cwd: () => ri.cwd ?? ri.request("cwd"),
		chdir() {
			throw new Error("process.chdir() is not supported in ri extensions");
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
		emitWarning: (warning) => ri.log("warn", [warning]),
		on(event, listener) {
			const list = listeners.get(event) ?? [];
			list.push(listener);
			listeners.set(event, list);
			return this;
		},
		once(event, listener) {
			return this.on(event, listener);
		},
		off(event, listener) {
			listeners.set(
				event,
				(listeners.get(event) ?? []).filter((item) => item !== listener),
			);
			return this;
		},
		removeListener(event, listener) {
			return this.off(event, listener);
		},
		removeAllListeners(event) {
			if (event) listeners.delete(event);
			else listeners.clear();
			return this;
		},
		emit(event, ...args) {
			for (const listener of listeners.get(event) ?? []) listener(...args);
			return (listeners.get(event) ?? []).length > 0;
		},
		listeners: (event) => [...(listeners.get(event) ?? [])],
		listenerCount: (event) => (listeners.get(event) ?? []).length,
		getBuiltinModule: (name) => globalThis.__ri_builtins[String(name).replace(/^node:/, "")],
		stdout: {
			isTTY: false,
			columns: 80,
			rows: 24,
			write(text) {
				ri.log("info", [String(text).replace(/\n$/, "")]);
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
				ri.log("warn", [String(text).replace(/\n$/, "")]);
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
			let done = false;
			return {
				getReader: () => ({
					read: async () => {
						if (done) return { done: true, value: undefined };
						done = true;
						return { done: false, value: bytes };
					},
					cancel: async () => {},
					releaseLock() {},
				}),
				async *[Symbol.asyncIterator]() {
					yield bytes;
				},
			};
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
		const result = await ri.op("fetch", {
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
